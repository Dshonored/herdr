//! Per-endpoint GUI state: the latest snapshot, the rendered pane surface, and
//! the endpoint request lane. Pure data plus message application, so it is
//! testable without sockets or a window.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::api::schema::AgentStatus;
use crate::client::endpoint::{
    decode_endpoint_control, ClientEndpointId, EndpointControlMessage, EndpointNegotiation,
    EndpointTransport as _, NativeEndpointTransport,
};
use crate::protocol::endpoint::{HEALTH_CHECK_CAPABILITY, HEALTH_PING_KIND};
use crate::protocol::{
    ClientMessage, ClientShellSnapshot, PaneSurfaceFrame, PaneSurfacePatch, ServerMessage,
};

use super::connection::{ConnectionGuard, HostTarget};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostStatus {
    Connecting,
    /// Remote setup in progress (checking, installing, or updating herdr).
    SettingUp(String),
    /// Updating needs to stop the machine's running herdr server; waiting for consent.
    NeedsRestart(String),
    Online,
    Offline(String),
}

/// Side effects of one server message that the window must perform.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HostEffects {
    pub(crate) redraw: bool,
    pub(crate) clipboard: Option<String>,
    pub(crate) notice: Option<String>,
}

struct InFlight {
    id: String,
    sent_at: Instant,
}

pub(crate) struct HostState {
    pub(crate) id: ClientEndpointId,
    pub(crate) label: String,
    pub(crate) target: HostTarget,
    pub(crate) status: HostStatus,
    pub(crate) generation: u64,
    pub(crate) snapshot: Option<ClientShellSnapshot>,
    /// Shared with the renderer; patches copy it only while a frame still holds it.
    pub(crate) surface: Option<Arc<PaneSurfaceFrame>>,
    pub(crate) retry_at: Option<Instant>,
    /// Consecutive failed connection attempts, for retry backoff.
    pub(crate) failures: u32,
    /// The user approved restarting this machine's server for the next attempt.
    pub(crate) allow_remote_restart: bool,
    pub(crate) expanded: bool,
    pub(crate) surface_active: bool,
    negotiation: Option<EndpointNegotiation>,
    transport: Option<NativeEndpointTransport>,
    guard: Option<ConnectionGuard>,
    queued: VecDeque<(String, String)>,
    in_flight: Option<InFlight>,
    response: Vec<u8>,
    next_request: u64,
    last_received: Instant,
}

impl HostState {
    pub(crate) fn new(id: ClientEndpointId, label: String, target: HostTarget) -> Self {
        Self {
            id,
            label,
            target,
            status: HostStatus::Connecting,
            generation: 0,
            snapshot: None,
            surface: None,
            retry_at: None,
            failures: 0,
            allow_remote_restart: false,
            expanded: true,
            surface_active: false,
            negotiation: None,
            transport: None,
            guard: None,
            queued: VecDeque::new(),
            in_flight: None,
            response: Vec::new(),
            next_request: 0,
            last_received: Instant::now(),
        }
    }

    /// Forgets the previous connection and returns the generation for a new attempt.
    pub(crate) fn begin_connect(&mut self, guard_slot: impl FnOnce(u64) -> ConnectionGuard) {
        self.disconnect(HostStatus::Connecting);
        self.generation += 1;
        self.guard = Some(guard_slot(self.generation));
    }

    pub(crate) fn connected(
        &mut self,
        transport: NativeEndpointTransport,
        negotiation: EndpointNegotiation,
    ) {
        self.transport = Some(transport);
        self.negotiation = Some(negotiation);
        self.status = HostStatus::Online;
        self.retry_at = None;
        self.failures = 0;
        self.allow_remote_restart = false;
        self.last_received = Instant::now();
    }

    pub(crate) fn disconnect(&mut self, status: HostStatus) {
        if let Some(mut transport) = self.transport.take() {
            transport.disconnect();
        }
        self.guard = None;
        self.negotiation = None;
        self.queued.clear();
        self.in_flight = None;
        self.response.clear();
        self.surface = None;
        self.status = status;
    }

    pub(crate) fn is_online(&self) -> bool {
        self.status == HostStatus::Online
    }

    pub(crate) fn supports_surface_interest(&self) -> bool {
        self.negotiation
            .as_ref()
            .is_some_and(EndpointNegotiation::supports_surface_interest)
    }

    /// Sends one message; a failed send marks the host offline.
    pub(crate) fn send(&mut self, message: &ClientMessage) {
        let Some(transport) = self.transport.as_mut() else {
            return;
        };
        let failed = transport
            .send(message)
            .err()
            .or_else(|| transport.take_error());
        if let Some(error) = failed {
            self.disconnect(HostStatus::Offline(error.to_string()));
        }
    }

    /// Queues one endpoint API request. Requests run one at a time per endpoint,
    /// like the TUI's command lane, so their effects apply in order.
    pub(crate) fn request(&mut self, method: &str, params: serde_json::Value) {
        self.next_request += 1;
        let id = format!("gui-{}", self.next_request);
        let request = serde_json::json!({ "id": id, "method": method, "params": params });
        self.queued.push_back((id, request.to_string()));
        self.pump_requests();
    }

    fn pump_requests(&mut self) {
        if self.transport.is_none() {
            return;
        }
        if self
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| in_flight.sent_at.elapsed() < REQUEST_TIMEOUT)
        {
            return;
        }
        self.in_flight = None;
        let Some(boot_id) = self.snapshot.as_ref().map(|s| s.boot_id.clone()) else {
            return;
        };
        let Some((id, request)) = self.queued.pop_front() else {
            return;
        };
        self.response.clear();
        self.in_flight = Some(InFlight {
            id,
            sent_at: Instant::now(),
        });
        self.send(&ClientMessage::ClientShellEndpointRequest { boot_id, request });
    }

    /// Periodic upkeep: request timeouts and health probes for idle bridges.
    pub(crate) fn tick(&mut self) {
        if !self.is_online() {
            return;
        }
        self.pump_requests();
        let wants_health = self
            .negotiation
            .as_ref()
            .is_some_and(|n| n.supports_capability(HEALTH_CHECK_CAPABILITY));
        if wants_health && self.last_received.elapsed() >= HEALTH_INTERVAL {
            self.last_received = Instant::now();
            self.send(&ClientMessage::EndpointControl {
                kind: HEALTH_PING_KIND.into(),
                data: "gui".into(),
            });
        }
    }

    pub(crate) fn set_surface_active(&mut self, active: bool) {
        if self.surface_active == active {
            return;
        }
        self.surface_active = active;
        if !active {
            self.surface = None;
        }
        if self.is_online() && self.supports_surface_interest() {
            self.request(
                "client_shell.surface.set",
                serde_json::json!({ "active": active }),
            );
        }
    }

    pub(crate) fn apply(&mut self, message: ServerMessage) -> HostEffects {
        self.last_received = Instant::now();
        let mut effects = HostEffects::default();
        match message {
            ServerMessage::ClientShellSnapshot(snapshot) => {
                self.snapshot = Some(*snapshot);
                effects.redraw = true;
                self.pump_requests();
            }
            ServerMessage::EndpointControl { kind, data } => {
                match decode_endpoint_control(&kind, &data) {
                    Ok(EndpointControlMessage::Snapshot(snapshot)) => {
                        self.snapshot = Some(*snapshot);
                        effects.redraw = true;
                        self.pump_requests();
                    }
                    Ok(_) => {}
                    Err(error) => effects.notice = Some(error),
                }
            }
            ServerMessage::PaneSurface(surface) => {
                if self.surface_active {
                    self.surface = Some(Arc::new(surface));
                    effects.redraw = true;
                }
            }
            ServerMessage::PaneSurfacePatch(patch) => {
                effects.redraw = self.apply_patch(&patch);
            }
            ServerMessage::ClientShellEndpointResponseChunk {
                request_id,
                final_chunk,
                data,
                ..
            } => {
                if self
                    .in_flight
                    .as_ref()
                    .is_some_and(|in_flight| in_flight.id == request_id)
                {
                    self.response.extend_from_slice(&data);
                    if final_chunk {
                        effects.notice = response_error(&self.response);
                        self.response.clear();
                        self.in_flight = None;
                        self.pump_requests();
                    }
                }
            }
            ServerMessage::ClientShellError { message } => effects.notice = Some(message),
            ServerMessage::SemanticNotification(notification) => {
                effects.notice = Some(match notification.body {
                    Some(body) => format!("{}: {body}", notification.title),
                    None => notification.title,
                });
            }
            ServerMessage::Clipboard { data } => {
                use base64::Engine as _;
                effects.clipboard = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok());
            }
            ServerMessage::ServerShutdown { reason } => {
                self.disconnect(HostStatus::Offline(
                    reason.unwrap_or_else(|| "server stopped".into()),
                ));
                effects.redraw = true;
            }
            _ => {}
        }
        effects
    }

    fn apply_patch(&mut self, patch: &PaneSurfacePatch) -> bool {
        let Some(surface) = self.surface.as_mut().map(Arc::make_mut) else {
            return false;
        };
        if patch.boot_id != surface.boot_id
            || patch.projection_revision != surface.projection_revision
            || patch.base_surface_revision != surface.surface_revision
        {
            return false;
        }
        let frame = &mut surface.frame;
        for row in &patch.rows {
            let start = usize::from(row.y) * usize::from(frame.width) + usize::from(row.x);
            let end = start + row.cells.len();
            if row.y >= frame.height
                || usize::from(row.x) + row.cells.len() > usize::from(frame.width)
                || end > frame.cells.len()
            {
                return false;
            }
            frame.cells[start..end].clone_from_slice(&row.cells);
        }
        for updated in &patch.panes {
            if let Some(existing) = surface
                .panes
                .iter_mut()
                .find(|pane| pane.pane_id == updated.pane_id)
            {
                *existing = updated.clone();
            }
        }
        surface.frame.cursor = patch.cursor.clone();
        surface.surface_revision = patch.surface_revision;
        true
    }

    /// Highest-priority agent status across the machine, for its sidebar dot.
    pub(crate) fn aggregate_status(&self) -> AgentStatus {
        self.snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.agent_status)
                    .max_by_key(|status| status_priority(*status))
                    .unwrap_or(AgentStatus::Unknown)
            })
            .unwrap_or(AgentStatus::Unknown)
    }
}

/// Delay before reconnecting after `failures` consecutive failures: 5s doubling to 60s.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    let seconds = 5u64.saturating_mul(1 << failures.saturating_sub(1).min(4));
    Duration::from_secs(seconds.min(60))
}

pub(crate) fn status_priority(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 4,
        AgentStatus::Done => 3,
        AgentStatus::Working => 2,
        AgentStatus::Idle => 1,
        AgentStatus::Unknown => 0,
    }
}

fn response_error(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let error = value.get("error")?;
    Some(
        error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CellData, FrameData, PaneSurfacePatchRow};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn host_with_surface() -> HostState {
        let mut host = HostState::new(ClientEndpointId::Local, "Local".into(), HostTarget::Local);
        host.surface_active = true;
        host.surface = Some(Arc::new(PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData {
                cells: (0..4).map(|_| cell(" ")).collect(),
                width: 2,
                height: 2,
                cursor: None,
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: Default::default(),
        }));
        host
    }

    fn patch(base: u64, x: u16, y: u16, cells: Vec<CellData>) -> PaneSurfacePatch {
        PaneSurfacePatch {
            boot_id: "boot".into(),
            projection_revision: 1,
            base_surface_revision: base,
            surface_revision: base + 1,
            rows: vec![PaneSurfacePatchRow { x, y, cells }],
            panes: Vec::new(),
            cursor: None,
        }
    }

    #[test]
    fn patch_applies_to_matching_base_revision() {
        let mut host = host_with_surface();
        assert!(host.apply_patch(&patch(1, 1, 1, vec![cell("x")])));
        let surface = host.surface.as_ref().unwrap();
        assert_eq!(surface.frame.cells[3].symbol, "x");
        assert_eq!(surface.surface_revision, 2);
    }

    #[test]
    fn patch_against_stale_revision_or_out_of_bounds_is_rejected() {
        let mut host = host_with_surface();
        assert!(!host.apply_patch(&patch(7, 0, 0, vec![cell("x")])));
        assert!(!host.apply_patch(&patch(1, 1, 0, vec![cell("x"), cell("y")])));
        assert!(!host.apply_patch(&patch(1, 0, 2, vec![cell("x")])));
        assert_eq!(host.surface.as_ref().unwrap().surface_revision, 1);
    }

    #[test]
    fn response_error_reads_the_api_error_message() {
        assert_eq!(
            response_error(br#"{"id":"gui-1","error":{"code":"x","message":"nope"}}"#),
            Some("nope".into())
        );
        assert_eq!(response_error(br#"{"id":"gui-1","result":{}}"#), None);
    }

    #[test]
    fn retry_delay_backs_off_to_a_minute() {
        assert_eq!(retry_delay(1), Duration::from_secs(5));
        assert_eq!(retry_delay(2), Duration::from_secs(10));
        assert_eq!(retry_delay(4), Duration::from_secs(40));
        assert_eq!(retry_delay(9), Duration::from_secs(60));
    }

    #[test]
    fn requests_wait_for_a_snapshot_boot_id() {
        let mut host = HostState::new(ClientEndpointId::Local, "Local".into(), HostTarget::Local);
        host.request("tab.create", serde_json::json!({}));
        assert!(host.in_flight.is_none());
        assert_eq!(host.queued.len(), 1);
    }
}
