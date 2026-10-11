//! The root window: machines and agents on the left, tabs and the terminal
//! surface on the right. All session behavior goes through the endpoint
//! protocol, so the GUI shows exactly what the TUI shows for the same server.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt as _;
use gpui::{
    canvas, div, prelude::*, px, Animation, AnimationExt, AnyElement, ClickEvent, ClipboardItem,
    Context, DragMoveEvent, FocusHandle, Hsla, KeyDownEvent, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollWheelEvent,
    SharedString, Timer, Window,
};
use tracing::warn;

use crate::api::schema::AgentStatus;
use crate::client::endpoint::{ClientEndpointId, EndpointCatalog, ProfileId};
use crate::protocol::{ClientMessage, ClientMouseKind, ClientPaneInputEvent, ClientSurfaceSize};

use super::claude_limits::{self, LimitsHook};
use super::connection::{
    spawn_connection, ConnectGeometry, ConnectionEvent, HostEvent, HostTarget,
};
use super::dialog::{Dialog, DialogOutcome, DialogSubmit};
use super::host::{retry_delay, status_priority, HostState, HostStatus};
use super::host_form::{HostForm, HostFormOutcome};
use super::machine_stats::{self, MachineStats};
use super::menu::{self, Command, ContextMenu, MenuItem};
use super::prefs::{GuiPrefs, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH};
use super::settings::{
    self, LimitsRow, MachineRow, SettingsSection, SettingsView, UpdateAction, UpdateView,
};
use super::terminal::{self, CellMetrics};
use super::theme::{status_label, Theme};
use super::updater::{self, UpdateStatus};
use super::usage::{AgentKind, LocalScanner};
use super::usage_view::{self, UsageState};

const USAGE_REFRESH: Duration = Duration::from_secs(60);
/// Gap between machine CPU/memory/network samples; each sample itself takes about a second.
const STATS_REFRESH: Duration = Duration::from_secs(4);

/// A tab being dragged by its grip to a new position.
struct DraggedTab {
    tab_id: String,
    index: usize,
}

/// A pane being dragged by its top grip onto another pane to swap them.
struct DraggedPane {
    pane_id: String,
}

/// What follows the pointer while dragging a tab or pane.
struct DragPreview {
    label: SharedString,
    bg: Hsla,
    fg: Hsla,
    border: Hsla,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Lifts out of its slot: fades in while settling 4px up, then follows the pointer.
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(6.0))
            .text_sm()
            .bg(self.bg)
            .text_color(self.fg)
            .border_1()
            .border_color(self.border)
            .shadow_lg()
            .child(div().text_xs().opacity(0.6).child("⋮⋮"))
            .child(self.label.clone())
            .with_animation(
                "drag-preview",
                Animation::new(Duration::from_millis(150)).with_easing(gpui::ease_out_quint()),
                |preview, t| preview.opacity(0.95 * t).mt(px(4.0 * (1.0 - t))),
            )
    }
}

fn drag_preview(
    label: impl Into<SharedString>,
    theme: &Theme,
    cx: &mut gpui::App,
) -> gpui::Entity<DragPreview> {
    let preview = DragPreview {
        label: label.into(),
        bg: theme.panel_bg,
        fg: theme.text,
        border: theme.border,
    };
    cx.new(|_| preview)
}

/// Whether the app shortcut modifier is down: Cmd on macOS, Ctrl+Shift elsewhere.
fn app_chord_held(modifiers: &gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform && !modifiers.control
    } else {
        modifiers.control && modifiers.shift
    }
}

/// Shortcut label shown on the `index`th tab (0-based) while the app modifier is held.
fn tab_shortcut_label(index: usize, count: usize) -> Option<String> {
    let prefix = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl+Shift+"
    };
    let number = match index {
        0..8 => index + 1,
        _ if index + 1 == count => 9,
        _ => return None,
    };
    Some(format!("{prefix}{number}"))
}

/// The `tab.move` index for dropping tab `from` on the left or right half of tab `onto`.
/// `tab.move` inserts before the index, counted before the moved tab is removed; `None`
/// when the drop would leave the tab where it is.
fn tab_insert_index(from: usize, onto: usize, after: bool) -> Option<usize> {
    let insert = onto + usize::from(after);
    (insert != from && insert != from + 1).then_some(insert)
}

/// Where the current tab or pane drag will land, drawn while dragging.
#[derive(Clone, Debug, PartialEq)]
enum DragTarget {
    Tab {
        from: usize,
        onto: usize,
        after: bool,
    },
    Pane {
        source: String,
        target: Option<String>,
    },
}

/// Drop indicators fade in quickly with a strong ease-out so they track the pointer.
fn landing_fade() -> Animation {
    Animation::new(Duration::from_millis(120)).with_easing(gpui::ease_out_quint())
}

/// The grip at the top centre of a pane, relative to the terminal's origin.
fn pane_grip_bounds(
    pane: &crate::protocol::PaneSurfacePane,
    metrics: &CellMetrics,
) -> gpui::Bounds<Pixels> {
    let (width, height) = (px(40.0), px(8.0));
    let center = metrics.cell_width * (f32::from(pane.rect.x) + f32::from(pane.rect.width) / 2.0);
    let top = metrics.line_height * f32::from(pane.rect.y);
    gpui::Bounds::new(
        gpui::point(center - width / 2.0, top),
        gpui::size(width, height),
    )
}
/// Fewest milliseconds between resizes sent while the window is being dragged; the TUI
/// client polls its terminal size at the same 100ms cadence.
const RESIZE_INTERVAL: Duration = Duration::from_millis(100);
/// Longest wait for the server to answer a resize before sending the next one.
const RESIZE_REPLY_TIMEOUT: Duration = Duration::from_millis(500);
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(5);
static LOGO_PNG: &[u8] = include_bytes!("../../assets/logo.png");
const NOTICE_DURATION: Duration = Duration::from_secs(6);
const DEFAULT_GRID: (u16, u16) = (120, 36);

pub(crate) struct GuiApp {
    hosts: Vec<HostState>,
    selected: usize,
    catalog: EndpointCatalog,
    theme: Theme,
    metrics: Option<CellMetrics>,
    grid: (u16, u16),
    terminal_focus: FocusHandle,
    form_focus: FocusHandle,
    terminal_origin: Rc<Cell<Point<Pixels>>>,
    events: UnboundedSender<HostEvent>,
    notice: Option<(String, Instant)>,
    host_form: Option<HostForm>,
    context_menu: Option<ContextMenu>,
    prefs: GuiPrefs,
    settings: Option<SettingsSection>,
    settings_focus: FocusHandle,
    usage: HashMap<ClientEndpointId, UsageState>,
    usage_expanded: HashSet<AgentKind>,
    claude_limits: HashMap<ClientEndpointId, LimitsHook>,
    /// Scanned host keys of machines waiting for first-connection trust.
    host_keys: HashMap<ClientEndpointId, Result<super::host_keys::ScannedKey, String>>,
    update_status: UpdateStatus,
    update_next: Instant,
    /// This computer's `[ui] max_fps`, read when settings open.
    local_max_fps: Option<u16>,
    /// Resolved herdr theme name, e.g. `tokyo-night`.
    theme_name: String,
    /// `[theme] auto_switch`: follow the system light/dark appearance.
    theme_auto: bool,
    dark_appearance: bool,
    usage_next: Instant,
    local_usage: Arc<Mutex<LocalScanner>>,
    /// Latest CPU/memory/network sample per machine.
    machine_stats: HashMap<ClientEndpointId, Result<MachineStats, String>>,
    stats_next: Instant,
    stats_loading: bool,
    /// The sidebar edge is being dragged.
    sidebar_dragging: bool,
    drag_target: Option<DragTarget>,
    logo: Arc<gpui::Image>,
    /// Open dialog and the machine its action applies to.
    dialog: Option<(usize, Dialog)>,
    mouse_pane: Option<String>,
    /// Text selected by dragging, and whether a selection drag is in progress.
    selection: Option<terminal::Selection>,
    selecting: bool,
    scroll_remainder: f32,
    /// `[ui] mouse_scroll_lines`: lines per mouse wheel notch, as in the terminal app.
    scroll_lines: f32,
    resize: ResizeThrottle,
}

impl GuiApp {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let catalog = EndpointCatalog::load().unwrap_or_else(|error| {
            warn!(%error, "saved SSH machines are unavailable");
            EndpointCatalog::default()
        });
        let mut hosts = vec![HostState::new(
            ClientEndpointId::Local,
            "Local".into(),
            HostTarget::Local,
        )];
        hosts.extend(catalog.ssh.iter().filter(|p| p.enabled).map(|profile| {
            HostState::new(
                ClientEndpointId::Ssh(profile.id.clone()),
                profile.label.clone(),
                HostTarget::Ssh {
                    profile_id: profile.id.to_string(),
                    target: profile.target.clone(),
                    session: profile.session.clone(),
                },
            )
        }));
        let selected = catalog
            .selected_profile
            .as_ref()
            .and_then(|selected| {
                hosts
                    .iter()
                    .position(|host| host.id == ClientEndpointId::Ssh(selected.clone()))
            })
            .unwrap_or(0);

        cx.background_executor()
            .spawn(async { updater::cleanup_previous_update() })
            .detach();
        let (events, receiver) = unbounded();
        Self::listen(receiver, cx);
        Self::start_ticker(cx);

        let terminal_focus = cx.focus_handle();
        window.focus(&terminal_focus);
        let mut app = Self {
            hosts,
            selected,
            catalog,
            theme: Theme::default(),
            metrics: None,
            grid: DEFAULT_GRID,
            terminal_focus,
            form_focus: cx.focus_handle(),
            terminal_origin: Rc::new(Cell::new(Point::default())),
            events,
            notice: None,
            host_form: None,
            context_menu: None,
            prefs: GuiPrefs::load(),
            settings: None,
            settings_focus: cx.focus_handle(),
            usage: HashMap::new(),
            usage_expanded: HashSet::new(),
            claude_limits: HashMap::new(),
            host_keys: HashMap::new(),
            update_status: UpdateStatus::Idle,
            update_next: Instant::now() + FIRST_UPDATE_CHECK,
            local_max_fps: None,
            theme_name: String::new(),
            theme_auto: false,
            dark_appearance: is_dark(window.appearance()),
            usage_next: Instant::now(),
            local_usage: Arc::new(Mutex::new(LocalScanner::default())),
            machine_stats: HashMap::new(),
            stats_next: Instant::now(),
            stats_loading: false,
            sidebar_dragging: false,
            drag_target: None,
            logo: Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                LOGO_PNG.to_vec(),
            )),
            dialog: None,
            mouse_pane: None,
            selection: None,
            selecting: false,
            scroll_remainder: 0.0,
            scroll_lines: crate::config::Config::load().config.ui.mouse_scroll_lines() as f32,
            resize: ResizeThrottle::default(),
        };
        app.reload_theme();
        cx.observe_window_appearance(window, |this, window, cx| {
            this.dark_appearance = is_dark(window.appearance());
            this.reload_theme();
            cx.notify();
        })
        .detach();
        for index in 0..app.hosts.len() {
            app.connect(index);
        }
        app
    }

    /// Applies this computer's herdr `[theme]` for the current system appearance.
    fn reload_theme(&mut self) {
        let config = crate::config::Config::load().config;
        let (palette, name) = crate::app::desktop_palette(&config, self.dark_appearance);
        self.theme = Theme::from_palette(&palette, self.dark_appearance);
        self.theme_name = name;
        self.theme_auto = config.theme.auto_switch;
    }

    /// Writes one `config.toml` value, then reloads the theme and the local server.
    fn change_config(
        &mut self,
        table: &'static str,
        key: &'static str,
        value: toml_edit::Value,
        done: String,
        cx: &mut Context<Self>,
    ) {
        let task = cx
            .background_executor()
            .spawn(async move { super::config_file::set(table, key, value) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.reload_theme();
                        this.local_max_fps = Some(crate::config::Config::load().config.ui.max_fps);
                        if this.hosts[0].is_online() {
                            this.hosts[0].request("server.reload_config", serde_json::json!({}));
                        }
                        this.show_notice(done);
                    }
                    Err(error) => this.show_notice(format!("could not save settings: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn set_theme(&mut self, name: &'static str, cx: &mut Context<Self>) {
        self.change_config("theme", "name", name.into(), format!("Theme: {name}"), cx);
    }

    pub(crate) fn set_theme_auto(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let done = if enabled {
            "Theme follows the system light/dark setting"
        } else {
            "Theme no longer follows the system light/dark setting"
        };
        self.change_config("theme", "auto_switch", enabled.into(), done.into(), cx);
    }

    fn listen(mut receiver: UnboundedReceiver<HostEvent>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.next().await {
                let mut batch = vec![first];
                while let Ok(event) = receiver.try_recv() {
                    batch.push(event);
                }
                if this
                    .update(cx, |this, cx| this.handle_events(batch, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_ticker(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            Timer::after(Duration::from_secs(1)).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();
    }

    fn geometry(&self) -> ConnectGeometry {
        let (cell_width_px, cell_height_px) = self
            .metrics
            .as_ref()
            .map(CellMetrics::pixel_size)
            .unwrap_or((0, 0));
        ConnectGeometry {
            size: ClientSurfaceSize {
                cols: self.grid.0,
                rows: self.grid.1,
            },
            cell_width_px,
            cell_height_px,
        }
    }

    fn connect(&mut self, index: usize) {
        let geometry = self.geometry();
        let active = index == self.selected;
        let events = self.events.clone();
        let Some(host) = self.hosts.get_mut(index) else {
            return;
        };
        host.surface_active = active;
        let (id, target) = (host.id.clone(), host.target.clone());
        let allow_restart = host.allow_remote_restart;
        host.begin_connect(|generation| {
            spawn_connection(
                id,
                target,
                generation,
                geometry,
                active,
                allow_restart,
                events,
            )
        });
    }

    fn handle_events(&mut self, batch: Vec<HostEvent>, cx: &mut Context<Self>) {
        let mut redraw = false;
        for HostEvent {
            host,
            generation,
            event,
        } in batch
        {
            let Some(index) = self.hosts.iter().position(|h| h.id == host) else {
                continue;
            };
            if self.hosts[index].generation != generation {
                continue;
            }
            redraw = true;
            match event {
                ConnectionEvent::Connected {
                    transport,
                    negotiation,
                } => {
                    self.hosts[index].connected(transport, negotiation);
                    self.run_limits_task(index, None, cx);
                    if index == self.selected {
                        self.send_resize(index);
                    }
                }
                ConnectionEvent::Message(message) => {
                    let effects = self.hosts[index].apply(*message);
                    if index == self.selected {
                        let size = self.hosts[index]
                            .surface
                            .as_ref()
                            .map(|surface| (surface.frame.width, surface.frame.height));
                        if size.is_some_and(|size| self.resize.answered(size)) {
                            self.flush_resize(cx);
                        }
                    }
                    if let Some(text) = effects.clipboard {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    if let Some(notice) = effects.notice {
                        self.show_notice(notice);
                    }
                }
                ConnectionEvent::Progress(message) => {
                    self.hosts[index].status = HostStatus::SettingUp(message);
                }
                ConnectionEvent::RestartRequired(message) => {
                    // No automatic retry: only the user may approve stopping remote panes.
                    let host = &mut self.hosts[index];
                    host.allow_remote_restart = false;
                    host.disconnect(HostStatus::NeedsRestart(message));
                }
                ConnectionEvent::Failed(error) => {
                    let host = &mut self.hosts[index];
                    warn!(machine = %host.label, %error, "machine connection failed");
                    host.allow_remote_restart = false;
                    let target = match &host.target {
                        HostTarget::Ssh { target, .. } => Some(target.clone()),
                        HostTarget::Local => None,
                    };
                    let shown = target.as_deref().unwrap_or(&host.label).to_owned();
                    let message = super::host_keys::explain(&error, &shown);
                    match (super::host_keys::classify(&error), target) {
                        // No retry: only the user can vouch for a machine's identity.
                        (super::host_keys::SshProblem::UnknownHostKey, Some(target)) => {
                            host.disconnect(HostStatus::NeedsTrust(message));
                            self.scan_host_key(index, target, cx);
                        }
                        _ => {
                            host.disconnect(HostStatus::Offline(message));
                            host.failures += 1;
                            host.retry_at = Some(Instant::now() + retry_delay(host.failures));
                        }
                    }
                }
                ConnectionEvent::Disconnected => {
                    let host = &mut self.hosts[index];
                    host.disconnect(HostStatus::Offline("connection closed".into()));
                    host.failures += 1;
                    host.retry_at = Some(Instant::now() + retry_delay(host.failures));
                }
            }
        }
        if redraw {
            cx.notify();
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        for index in 0..self.hosts.len() {
            self.hosts[index].tick();
            if self.hosts[index].retry_at.is_some_and(|at| at <= now) {
                self.hosts[index].retry_at = None;
                self.connect(index);
                cx.notify();
            }
        }
        // Release builds check on their own; local builds only when asked.
        if now >= self.update_next {
            self.update_next = now + UPDATE_CHECK_INTERVAL;
            if updater::current_version().is_some() {
                self.check_for_updates(false, cx);
            }
        }
        if now >= self.stats_next && self.refresh_machine_stats(cx) {
            self.stats_next = now + STATS_REFRESH;
        }
        // A machine that is still connecting is retried every tick until it can be read.
        if self.prefs.usage_enabled
            && now >= self.usage_next
            && self.refresh_usage(self.selected, cx)
        {
            self.usage_next = now + USAGE_REFRESH;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= NOTICE_DURATION)
        {
            self.notice = None;
            cx.notify();
        }
    }

    fn show_notice(&mut self, notice: String) {
        self.notice = Some((notice, Instant::now()));
    }

    fn selected_host(&self) -> &HostState {
        &self.hosts[self.selected]
    }

    fn select_host(&mut self, index: usize) {
        if index == self.selected || index >= self.hosts.len() {
            return;
        }
        let previous = self.selected;
        self.selected = index;
        self.selection = None;
        self.usage_next = Instant::now();
        self.stats_next = Instant::now();
        if self.hosts[previous].supports_surface_interest() {
            self.hosts[previous].set_surface_active(false);
        }
        let host = &mut self.hosts[index];
        if host.is_online() && !host.supports_surface_interest() && !host.surface_active {
            // Old servers cannot toggle surface interest; reconnect as the active surface.
            self.connect(index);
        } else {
            host.set_surface_active(true);
            self.send_resize(index);
        }
        let id = self.hosts[index].id.clone();
        if self.catalog.select_endpoint(&id) {
            if let Err(error) = self.catalog.store_selection() {
                warn!(%error, "could not save the selected machine");
            }
        }
    }

    fn send_resize(&mut self, index: usize) {
        let geometry = self.geometry();
        self.hosts[index].send(&ClientMessage::ClientShellResize {
            cell_width_px: geometry.cell_width_px,
            cell_height_px: geometry.cell_height_px,
            surface_size: geometry.size,
            pixel_mouse: false,
        });
    }

    fn resize_terminal(&mut self, cols: u16, rows: u16, cx: &mut Context<Self>) {
        if self.grid == (cols, rows) {
            return;
        }
        self.grid = (cols, rows);
        self.resize.pending = true;
        self.flush_resize(cx);
        cx.notify();
    }

    /// Sends the latest size when the throttle allows it, otherwise schedules a retry.
    /// Every resize makes the server re-lay out and redraw every pane, so sending one per
    /// frame while dragging floods slow (SSH) connections with full-screen frames.
    fn flush_resize(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        match self.resize.ready_at(now) {
            None => {}
            Some(at) if at <= now => {
                self.resize.sent(now, self.grid);
                self.send_resize(self.selected);
            }
            Some(at) => {
                if self.resize.retry_scheduled {
                    return;
                }
                self.resize.retry_scheduled = true;
                cx.spawn(async move |this, cx| {
                    Timer::after(at.saturating_duration_since(Instant::now())).await;
                    let _ = this.update(cx, |this, cx| {
                        this.resize.retry_scheduled = false;
                        this.flush_resize(cx);
                    });
                })
                .detach();
            }
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) {
        let host = &mut self.hosts[self.selected];
        if host.is_online() {
            host.request(method, params);
        } else {
            let notice = format!("{} is not connected", host.label);
            self.show_notice(notice);
        }
    }

    fn focused_ids(&self) -> (Option<String>, Option<String>, Option<String>) {
        self.selected_host()
            .snapshot
            .as_ref()
            .map(|s| {
                (
                    s.focused_workspace_id.clone(),
                    s.focused_tab_id.clone(),
                    s.focused_pane_id.clone(),
                )
            })
            .unwrap_or_default()
    }

    fn send_pane_input(&mut self, pane_id: String, events: Vec<ClientPaneInputEvent>) {
        self.hosts[self.selected].send(&ClientMessage::ClientShellPaneInput { pane_id, events });
    }

    // ----- actions ---------------------------------------------------------

    fn new_tab(&mut self) {
        let (workspace_id, _, _) = self.focused_ids();
        self.request(
            "tab.create",
            serde_json::json!({ "workspace_id": workspace_id, "focus": true }),
        );
    }

    /// Moves a dragged tab to where another tab sits.
    fn drop_tab(&mut self, dragged: &DraggedTab, target: usize) {
        let after = matches!(
            self.drag_target.take(),
            Some(DragTarget::Tab { onto, after: true, .. }) if onto == target
        );
        let Some(insert_index) = tab_insert_index(dragged.index, target, after) else {
            return;
        };
        self.request(
            "tab.move",
            serde_json::json!({ "tab_id": dragged.tab_id, "insert_index": insert_index }),
        );
    }

    /// Whether `position` (window coordinates) is on a pane's drag grip.
    fn on_pane_grip(&self, position: Point<Pixels>) -> bool {
        let (Some(surface), Some(metrics)) = (&self.selected_host().surface, &self.metrics) else {
            return false;
        };
        let origin = self.terminal_origin.get();
        let local = gpui::point(position.x - origin.x, position.y - origin.y);
        surface.panes.len() > 1
            && surface
                .panes
                .iter()
                .any(|pane| pane_grip_bounds(pane, metrics).contains(&local))
    }

    /// Swaps a dragged pane with the pane under the pointer.
    fn pane_id_at(&self, position: Point<Pixels>) -> Option<String> {
        let (col, row) = self.mouse_cell(position)?;
        let surface = self.selected_host().surface.as_ref()?;
        terminal::pane_at(&surface.panes, col, row).map(|pane| pane.pane_id.clone())
    }

    fn set_drag_target(&mut self, target: Option<DragTarget>, cx: &mut Context<Self>) {
        if self.drag_target != target {
            self.drag_target = target;
            cx.notify();
        }
    }

    fn drop_pane(&mut self, dragged: &DraggedPane, position: Point<Pixels>) {
        self.drag_target = None;
        let target = self.pane_id_at(position);
        if let Some(target) = target.filter(|target| *target != dragged.pane_id) {
            self.request(
                "pane.swap",
                serde_json::json!({ "source_pane_id": dragged.pane_id, "target_pane_id": target }),
            );
        }
    }

    /// Focuses the `number`th tab (1-based) of the focused workspace; 9 is the last tab,
    /// as in browsers.
    fn focus_tab_number(&mut self, number: u8) {
        let Some(snapshot) = &self.selected_host().snapshot else {
            return;
        };
        let workspace_id = snapshot.focused_workspace_id.as_ref();
        let tabs: Vec<_> = snapshot
            .tabs
            .iter()
            .filter(|tab| Some(&tab.workspace_id) == workspace_id)
            .collect();
        let tab = if number == 9 {
            tabs.last()
        } else {
            tabs.get(usize::from(number) - 1)
        };
        if let Some(tab) = tab {
            let tab_id = tab.tab_id.clone();
            self.request("tab.focus", serde_json::json!({ "tab_id": tab_id }));
        }
    }

    fn new_workspace(&mut self) {
        let (workspace_id, _, _) = self.focused_ids();
        self.request(
            "workspace.create",
            serde_json::json!({ "source_workspace_id": workspace_id, "focus": true }),
        );
    }

    fn split(&mut self, direction: &str) {
        let (_, _, pane_id) = self.focused_ids();
        self.request(
            "pane.split",
            serde_json::json!({ "target_pane_id": pane_id, "direction": direction, "focus": true }),
        );
    }

    fn close_pane(&mut self) {
        if let (_, _, Some(pane_id)) = self.focused_ids() {
            self.request("pane.close", serde_json::json!({ "pane_id": pane_id }));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let (_, _, Some(pane_id)) = self.focused_ids() {
            self.send_pane_input(pane_id, vec![ClientPaneInputEvent::Paste(text)]);
        }
    }

    fn remove_host(&mut self, index: usize) {
        let ClientEndpointId::Ssh(profile_id) = self.hosts[index].id.clone() else {
            return;
        };
        self.drop_host(index);
        self.catalog.remove_ssh(&profile_id);
        if let Err(error) = self.catalog.store_profiles() {
            self.show_notice(format!("could not save machines: {error}"));
        }
    }

    /// Disconnects a machine and removes it from the window (not from the catalog).
    fn drop_host(&mut self, index: usize) {
        if index == 0 || index >= self.hosts.len() {
            return;
        }
        if self.selected == index {
            self.select_host(0);
        }
        self.hosts[index].disconnect(HostStatus::Offline("removed".into()));
        let id = self.hosts.remove(index).id;
        self.usage.remove(&id);
        self.machine_stats.remove(&id);
        if self.selected > index {
            self.selected -= 1;
        }
    }

    fn add_host(&mut self, label: String, target: String, session: String) -> Result<(), String> {
        let id: ProfileId = self
            .catalog
            .add_ssh(label.clone(), target.clone(), session.clone())?;
        if let Err(error) = self.catalog.store_profiles() {
            self.catalog.remove_ssh(&id);
            return Err(format!("could not save machines: {error}"));
        }
        self.hosts.push(HostState::new(
            ClientEndpointId::Ssh(id.clone()),
            label,
            HostTarget::Ssh {
                profile_id: id.to_string(),
                target,
                session,
            },
        ));
        let index = self.hosts.len() - 1;
        self.connect(index);
        Ok(())
    }

    // ----- input -----------------------------------------------------------

    fn on_terminal_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        if self.context_menu.is_some() {
            // An open menu owns the keyboard; Escape dismisses it.
            if keystroke.key == "escape" {
                self.close_menu();
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        let modifiers = &keystroke.modifiers;
        if app_chord_held(modifiers) {
            let handled = match keystroke.key.as_str() {
                "v" => {
                    self.paste(cx);
                    true
                }
                "c" if self.selection.is_some() => {
                    self.copy_selection(cx);
                    true
                }
                "t" => {
                    self.new_tab();
                    true
                }
                "n" => {
                    self.new_workspace();
                    true
                }
                "d" => {
                    self.split("right");
                    true
                }
                "e" => {
                    self.split("down");
                    true
                }
                "w" => {
                    self.close_pane();
                    true
                }
                key @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                    self.focus_tab_number(key.as_bytes()[0] - b'0');
                    true
                }
                _ => false,
            };
            if handled {
                cx.stop_propagation();
                cx.notify();
                return;
            }
        }
        let Some(input) = terminal::key_input(keystroke, event.is_held) else {
            return;
        };
        if self.selection.take().is_some() {
            cx.notify();
        }
        if let (_, _, Some(pane_id)) = self.focused_ids() {
            self.send_pane_input(pane_id, vec![input]);
        }
        cx.stop_propagation();
    }

    fn mouse_cell(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let metrics = self.metrics.as_ref()?;
        Some(metrics.cell_at(self.terminal_origin.get(), position))
    }

    /// Starts selecting text on a left press: always with Shift, otherwise only in panes
    /// whose program doesn't use the mouse. False when the press belongs to the program.
    fn start_selection(&mut self, position: Point<Pixels>, modifiers: &gpui::Modifiers) -> bool {
        self.selection = None;
        let Some((col, row)) = self.mouse_cell(position) else {
            return false;
        };
        let Some(surface) = self.selected_host().surface.clone() else {
            return false;
        };
        let Some(pane) = terminal::pane_at(&surface.panes, col, row) else {
            return false;
        };
        if pane.mouse_reporting && !modifiers.shift {
            return false;
        }
        if !pane.focused {
            let pane_id = pane.pane_id.clone();
            self.request("pane.focus", serde_json::json!({ "pane_id": pane_id }));
        }
        self.selection = Some(terminal::Selection::start(pane, col, row));
        self.selecting = true;
        true
    }

    fn link_at(&self, position: Point<Pixels>) -> Option<String> {
        let (col, row) = self.mouse_cell(position)?;
        let surface = self.selected_host().surface.as_ref()?;
        terminal::link_at(&surface.frame, col, row)
    }

    fn extend_selection(&mut self, position: Point<Pixels>) {
        let cell = self.mouse_cell(position);
        if let (Some(selection), Some((col, row))) = (self.selection.as_mut(), cell) {
            selection.extend(col, row);
        }
    }

    /// Ends a selection drag; a non-empty selection is copied, like most terminals.
    fn finish_selection(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.is_empty())
        {
            self.selection = None;
        } else {
            self.copy_selection(cx);
        }
        cx.notify();
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let host = self.selected_host();
        let (Some(selection), Some(surface)) = (&self.selection, &host.surface) else {
            return;
        };
        let text = selection.text(&surface.frame);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.show_notice("Copied selection".into());
        }
    }

    fn on_terminal_mouse(
        &mut self,
        kind: ClientMouseKind,
        position: Point<Pixels>,
        modifiers: &gpui::Modifiers,
    ) {
        if self.modal_open() {
            return;
        }
        let Some((col, row)) = self.mouse_cell(position) else {
            return;
        };
        let Some(surface) = self.selected_host().surface.clone() else {
            return;
        };
        let pane = match (&kind, &self.mouse_pane) {
            // Drags and releases stay with the pane that received the press.
            (ClientMouseKind::Drag(_) | ClientMouseKind::Up(_), Some(pane_id)) => {
                surface.panes.iter().find(|pane| &pane.pane_id == pane_id)
            }
            _ => terminal::pane_at(&surface.panes, col, row),
        };
        let Some(pane) = pane else {
            return;
        };
        if let ClientMouseKind::Down(_) = kind {
            self.mouse_pane = Some(pane.pane_id.clone());
            if !pane.focused {
                let pane_id = pane.pane_id.clone();
                self.request("pane.focus", serde_json::json!({ "pane_id": pane_id }));
            }
        }
        if matches!(kind, ClientMouseKind::Up(_)) {
            self.mouse_pane = None;
        }
        if pane.mouse_reporting {
            let event = terminal::mouse_input(pane, kind, col, row, modifiers, 1);
            let pane_id = pane.pane_id.clone();
            self.send_pane_input(pane_id, vec![event]);
        }
    }

    fn on_terminal_scroll(&mut self, event: &ScrollWheelEvent) {
        let Some(metrics) = self.metrics.as_ref() else {
            return;
        };
        let lines = match event.delta {
            ScrollDelta::Lines(delta) => delta.y * self.scroll_lines,
            ScrollDelta::Pixels(delta) => delta.y / metrics.line_height,
        } + self.scroll_remainder;
        let whole = lines.trunc();
        self.scroll_remainder = lines - whole;
        if whole == 0.0 {
            return;
        }
        let Some((col, row)) = self.mouse_cell(event.position) else {
            return;
        };
        let Some(surface) = self.selected_host().surface.clone() else {
            return;
        };
        let Some(pane) = terminal::pane_at(&surface.panes, col, row) else {
            return;
        };
        let kind = if whole > 0.0 {
            ClientMouseKind::ScrollUp
        } else {
            ClientMouseKind::ScrollDown
        };
        let count = whole.abs().min(f32::from(u16::MAX)) as u16;
        let input = terminal::mouse_input(pane, kind, col, row, &event.modifiers, count);
        self.send_pane_input(pane.pane_id.clone(), vec![input]);
    }

    pub(crate) fn on_form_key_event(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.host_form.as_mut() else {
            return;
        };
        let outcome = form.key(&event.keystroke);
        self.finish_form(outcome, window);
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn focus_form_field(&mut self, index: usize) {
        if let Some(form) = self.host_form.as_mut() {
            form.focus_field(index);
        }
    }

    pub(crate) fn cancel_form(&mut self, window: &mut Window) {
        self.close_form(window);
    }

    pub(crate) fn submit_form(&mut self, window: &mut Window) {
        if let Some(form) = self.host_form.as_mut() {
            let outcome = form.force_submit();
            self.finish_form(outcome, window);
        }
    }

    fn finish_form(&mut self, outcome: HostFormOutcome, window: &mut Window) {
        match outcome {
            HostFormOutcome::Pending => {}
            HostFormOutcome::Cancel => self.close_form(window),
            HostFormOutcome::Submit {
                label,
                target,
                session,
            } => match self.add_host(label, target, session) {
                Ok(()) => self.close_form(window),
                Err(error) => {
                    if let Some(form) = self.host_form.as_mut() {
                        form.error = Some(error);
                    }
                }
            },
        }
    }

    fn modal_open(&self) -> bool {
        self.host_form.is_some()
            || self.context_menu.is_some()
            || self.dialog.is_some()
            || self.settings.is_some()
    }

    /// Where keyboard focus returns when a dialog or form closes.
    fn restore_focus(&self, window: &mut Window) {
        if self.settings.is_some() {
            window.focus(&self.settings_focus);
        } else {
            window.focus(&self.terminal_focus);
        }
    }

    // ----- machine stats ---------------------------------------------------

    /// Samples the selected machine; false when it can't be read yet.
    fn refresh_machine_stats(&mut self, cx: &mut Context<Self>) -> bool {
        if self.stats_loading {
            return true;
        }
        let host = self.selected_host();
        let target = match &host.target {
            HostTarget::Local => None,
            HostTarget::Ssh { target, .. } if host.is_online() => Some(target.clone()),
            HostTarget::Ssh { .. } => return false,
        };
        let id = host.id.clone();
        self.stats_loading = true;
        let sample = cx
            .background_executor()
            .spawn(async move { machine_stats::sample(target.as_deref()) });
        cx.spawn(async move |this, cx| {
            let result = sample.await.map_err(|error| error.to_string());
            let _ = this.update(cx, |this, cx| {
                this.stats_loading = false;
                this.machine_stats.insert(id, result);
                cx.notify();
            });
        })
        .detach();
        true
    }

    // ----- usage -----------------------------------------------------------

    /// Starts a usage scan; false when the machine cannot be read yet.
    fn refresh_usage(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(host) = self.hosts.get(index) else {
            return false;
        };
        let id = host.id.clone();
        let target = match &host.target {
            HostTarget::Local => None,
            HostTarget::Ssh { target, .. } => {
                if !self.prefs.remote_usage || !host.is_online() {
                    return false;
                }
                Some(target.clone())
            }
        };
        let state = self.usage.entry(id.clone()).or_default();
        if state.loading {
            return true;
        }
        state.loading = true;
        let scanner = self.local_usage.clone();
        let scan = cx.background_executor().spawn(async move {
            match target {
                None => scanner
                    .lock()
                    .map(|mut scanner| scanner.scan())
                    .map_err(|_| "usage scanner unavailable".to_owned()),
                Some(target) => super::usage::scan_remote(&target).map_err(|e| e.to_string()),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = scan.await;
            let _ = this.update(cx, |this, cx| {
                let state = this.usage.entry(id).or_default();
                state.loading = false;
                match result {
                    Ok(report) => {
                        state.report = Some(report);
                        state.error = None;
                    }
                    Err(error) => state.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    fn scan_host_key(&mut self, index: usize, target: String, cx: &mut Context<Self>) {
        let id = self.hosts[index].id.clone();
        self.host_keys.remove(&id);
        let task = cx
            .background_executor()
            .spawn(async move { super::host_keys::scan(&target) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.host_keys.insert(id, result);
                cx.notify();
            });
        })
        .detach();
    }

    /// Shows the scanned fingerprint and asks before recording it.
    fn open_trust_dialog(&mut self, index: usize, window: &mut Window) {
        let host = &self.hosts[index];
        match self.host_keys.get(&host.id) {
            None => self.show_notice(format!("Reading {}'s host key…", host.label)),
            Some(Err(error)) => {
                let notice = format!("Couldn't read {}'s host key: {error}", host.label);
                self.show_notice(notice);
            }
            Some(Ok(key)) => {
                let message = format!(
                    "This computer has never connected to {} ({}). Its host key fingerprint is:

{}

Trust it only if it matches the machine. On the machine itself, `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` prints its fingerprint.",
                    host.label,
                    key.host,
                    key.fingerprints.join("
")
                );
                self.dialog = Some((
                    index,
                    Dialog::confirm(
                        format!("Trust {}?", host.label),
                        &message,
                        "Trust & connect",
                        false,
                        Command::TrustHostKey,
                    ),
                ));
                window.focus(&self.form_focus);
            }
        }
    }

    fn trust_host_key(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(Ok(key)) = self.host_keys.get(&self.hosts[index].id).cloned() else {
            return;
        };
        let task = cx
            .background_executor()
            .spawn(async move { super::host_keys::trust(&key) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        let host = &mut this.hosts[index];
                        this.host_keys.remove(&host.id);
                        host.failures = 0;
                        this.connect(index);
                    }
                    Err(error) => this.show_notice(format!("couldn't save the host key: {error}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn toggle_usage_details(&mut self, agent: AgentKind) {
        if !self.usage_expanded.remove(&agent) {
            self.usage_expanded.insert(agent);
        }
    }

    // ----- settings ----------------------------------------------------------

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.settings = Some(self.settings.unwrap_or_default());
        window.focus(&self.settings_focus);
        self.check_claude_limits(cx);
        self.local_max_fps = Some(crate::config::Config::load().config.ui.max_fps);
    }

    // ----- frame rate ----------------------------------------------------------

    pub(crate) fn set_frame_rate(&mut self, fps: u16, cx: &mut Context<Self>) {
        self.change_config(
            "ui",
            "max_fps",
            i64::from(fps).into(),
            format!("Terminal frame rate set to {fps} fps"),
            cx,
        );
    }

    // ----- updates -------------------------------------------------------------

    pub(crate) fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if matches!(
            self.update_status,
            UpdateStatus::Checking | UpdateStatus::Downloading(_) | UpdateStatus::Installing
        ) || matches!(self.update_status, UpdateStatus::Ready(_)) && !manual
        {
            return;
        }
        self.update_status = UpdateStatus::Checking;
        let task = cx
            .background_executor()
            .spawn(async { updater::fetch_latest() });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(release) if updater::is_newer(&release) => {
                        this.update_status = UpdateStatus::Available(release);
                        if this.prefs.auto_download_updates && !manual {
                            this.download_update(cx);
                        }
                    }
                    Ok(_) => {
                        this.update_status = UpdateStatus::UpToDate;
                        if manual {
                            this.show_notice("herdr desktop is up to date".into());
                        }
                    }
                    Err(error) => {
                        if manual {
                            this.show_notice(format!("update check failed: {error}"));
                        }
                        this.update_status = UpdateStatus::Failed(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn download_update(&mut self, cx: &mut Context<Self>) {
        let UpdateStatus::Available(release) = self.update_status.clone() else {
            return;
        };
        self.update_status = UpdateStatus::Downloading(release.clone());
        let task = cx
            .background_executor()
            .spawn(async move { updater::download(&release) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.update_status = match result {
                    Ok(staged) => UpdateStatus::Ready(staged),
                    Err(error) => {
                        this.show_notice(format!("update download failed: {error}"));
                        UpdateStatus::Failed(error)
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        let UpdateStatus::Ready(staged) = self.update_status.clone() else {
            return;
        };
        self.update_status = UpdateStatus::Installing;
        let task = cx.background_executor().spawn(async move {
            updater::install(&staged)?;
            updater::relaunch()
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(error) => {
                    this.show_notice(format!("update failed: {error}"));
                    this.update_status = UpdateStatus::Failed(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn open_release_notes(&mut self, cx: &mut Context<Self>) {
        let url = match &self.update_status {
            UpdateStatus::Available(release) | UpdateStatus::Downloading(release) => {
                release.page.clone()
            }
            UpdateStatus::Ready(staged) => staged.release.page.clone(),
            _ => updater::releases_page(),
        };
        cx.open_url(&url);
    }

    fn update_view(&self) -> UpdateView {
        let (status, action) = match &self.update_status {
            UpdateStatus::Idle => ("Not checked yet".to_owned(), None),
            UpdateStatus::Checking => ("Checking for updates…".to_owned(), None),
            UpdateStatus::UpToDate => ("Up to date".to_owned(), None),
            UpdateStatus::Available(release) => (
                format!("{} is available", release.version),
                Some(UpdateAction::Download),
            ),
            UpdateStatus::Downloading(release) => {
                (format!("Downloading {}…", release.version), None)
            }
            UpdateStatus::Ready(staged) => (
                format!("{} is downloaded and verified", staged.release.version),
                Some(UpdateAction::Restart),
            ),
            UpdateStatus::Installing => ("Installing…".to_owned(), None),
            UpdateStatus::Failed(error) => (format!("Last check failed: {error}"), None),
        };
        UpdateView {
            current: updater::current_version_label(),
            status,
            action,
            busy: matches!(self.update_status, UpdateStatus::Checking),
        }
    }

    fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = &self.theme;
        let (message, action): (String, Option<UpdateAction>) = match &self.update_status {
            UpdateStatus::Available(release) => (
                format!("herdr desktop {} is available", release.version),
                Some(UpdateAction::Download),
            ),
            UpdateStatus::Downloading(release) => (
                format!("Downloading herdr desktop {}…", release.version),
                None,
            ),
            UpdateStatus::Ready(staged) => (
                format!("herdr desktop {} is ready", staged.release.version),
                Some(UpdateAction::Restart),
            ),
            UpdateStatus::Installing => ("Installing update…".to_owned(), None),
            _ => return None,
        };
        let mut banner = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(6.0))
            .text_sm()
            .bg(theme.selected)
            .border_b_1()
            .border_color(theme.border)
            .child(div().flex_1().min_w_0().truncate().child(message));
        if let Some(action) = action {
            let label = match action {
                UpdateAction::Download => "Download",
                UpdateAction::Restart => "Restart to update",
            };
            banner = banner.child(
                toolbar_button("update-banner-action", label, theme)
                    .text_color(theme.accent)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        match action {
                            UpdateAction::Download => this.download_update(cx),
                            UpdateAction::Restart => this.restart_to_update(cx),
                        }
                        cx.notify();
                    })),
            );
        }
        Some(banner.into_any_element())
    }

    /// Looks up whether each connected machine captures Claude Code plan limits.
    fn check_claude_limits(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.hosts.len() {
            self.run_limits_task(index, None, cx);
        }
    }

    pub(crate) fn set_claude_limits(&mut self, index: usize, enable: bool, cx: &mut Context<Self>) {
        self.run_limits_task(index, Some(enable), cx);
    }

    /// Checks (`change: None`) or enables/disables limit capture on one machine.
    fn run_limits_task(&mut self, index: usize, change: Option<bool>, cx: &mut Context<Self>) {
        let Some(host) = self.hosts.get(index) else {
            return;
        };
        let target = match &host.target {
            HostTarget::Local => None,
            HostTarget::Ssh { .. } if !host.is_online() => return,
            HostTarget::Ssh { target, .. } => Some(target.clone()),
        };
        let id = host.id.clone();
        if change.is_none() && self.claude_limits.get(&id) == Some(&LimitsHook::Checking) {
            return;
        }
        self.claude_limits.insert(id.clone(), LimitsHook::Checking);
        let task = cx.background_executor().spawn(async move {
            let applied = match (&target, change) {
                (_, None) => Ok(()),
                (None, Some(enable)) => claude_limits::local_set(enable),
                (Some(target), Some(enable)) => claude_limits::remote_set(target, enable),
            };
            applied.and_then(|()| match &target {
                None => claude_limits::local_status(),
                Some(target) => claude_limits::remote_status(target),
            })
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = match result {
                    Ok(true) => LimitsHook::Enabled,
                    Ok(false) => LimitsHook::Disabled,
                    Err(error) => LimitsHook::Failed(error),
                };
                this.claude_limits.insert(id, state);
                if change.is_some() {
                    this.usage_next = Instant::now();
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn limits_rows(&self) -> Vec<LimitsRow> {
        self.hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| matches!(host.target, HostTarget::Local) || host.is_online())
            .map(|(index, host)| LimitsRow {
                host: index,
                label: host.label.clone(),
                state: self.claude_limits.get(&host.id).cloned(),
            })
            .collect()
    }

    pub(crate) fn open_limits_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = Some(SettingsSection::Usage);
        self.open_settings(window, cx);
    }

    pub(crate) fn close_settings(&mut self, window: &mut Window) {
        self.settings = None;
        window.focus(&self.terminal_focus);
    }

    pub(crate) fn set_settings_section(&mut self, section: SettingsSection) {
        self.settings = Some(section);
    }

    pub(crate) fn update_prefs(&mut self, change: impl FnOnce(&mut GuiPrefs)) {
        let before = self.prefs.clone();
        change(&mut self.prefs);
        if self.prefs.font_size != before.font_size {
            // Re-measured on the next frame; the terminal then resizes to the new grid.
            self.metrics = None;
        }
        if self.prefs.usage_enabled && !before.usage_enabled
            || self.prefs.remote_usage != before.remote_usage
        {
            self.usage.clear();
            self.usage_next = Instant::now();
        }
        if let Err(error) = self.prefs.store() {
            self.show_notice(format!("could not save settings: {error}"));
        }
    }

    fn machine_rows(&self) -> Vec<MachineRow> {
        self.catalog
            .ssh
            .iter()
            .map(|profile| {
                let host = self
                    .hosts
                    .iter()
                    .find(|host| host.id == ClientEndpointId::Ssh(profile.id.clone()));
                MachineRow {
                    profile_id: profile.id.to_string(),
                    label: profile.label.clone(),
                    target: profile.target.clone(),
                    session: profile.session.clone(),
                    enabled: profile.enabled,
                    status: host.map(|host| match &host.status {
                        HostStatus::Online => "online".to_owned(),
                        HostStatus::Connecting => "connecting".to_owned(),
                        HostStatus::SettingUp(message)
                        | HostStatus::NeedsRestart(message)
                        | HostStatus::NeedsTrust(message)
                        | HostStatus::Offline(message) => message.clone(),
                    }),
                }
            })
            .collect()
    }

    fn host_index_for_profile(&self, profile_id: &str) -> Option<usize> {
        self.hosts.iter().position(
            |host| matches!(&host.id, ClientEndpointId::Ssh(id) if id.as_str() == profile_id),
        )
    }

    pub(crate) fn set_machine_enabled(&mut self, profile_id: &str, enabled: bool) {
        let Ok(id) = ProfileId::parse(profile_id) else {
            return;
        };
        if !self.catalog.set_enabled(&id, enabled) {
            return;
        }
        if let Err(error) = self.catalog.store_profiles() {
            self.show_notice(format!("could not save machines: {error}"));
        }
        match (enabled, self.host_index_for_profile(profile_id)) {
            (true, None) => {
                let Some(profile) = self.catalog.ssh.iter().find(|p| p.id == id) else {
                    return;
                };
                self.hosts.push(HostState::new(
                    ClientEndpointId::Ssh(id.clone()),
                    profile.label.clone(),
                    HostTarget::Ssh {
                        profile_id: id.to_string(),
                        target: profile.target.clone(),
                        session: profile.session.clone(),
                    },
                ));
                self.connect(self.hosts.len() - 1);
            }
            (false, Some(index)) => self.drop_host(index),
            _ => {}
        }
    }

    pub(crate) fn rename_machine_dialog(&mut self, profile_id: &str, window: &mut Window) {
        let Some(index) = self.host_index_for_profile(profile_id) else {
            return;
        };
        let label = self.hosts[index].label.clone();
        self.dialog = Some((
            index,
            Dialog::input(
                "Rename machine",
                "Name",
                &label,
                "Rename",
                DialogSubmit::RenameMachine,
            ),
        ));
        window.focus(&self.form_focus);
    }

    pub(crate) fn remove_machine_dialog(&mut self, profile_id: &str, window: &mut Window) {
        let label = self
            .catalog
            .ssh
            .iter()
            .find(|profile| profile.id.as_str() == profile_id)
            .map(|profile| profile.label.clone())
            .unwrap_or_else(|| "this machine".into());
        self.dialog = Some((
            0,
            Dialog::confirm(
                format!("Remove {label}?"),
                "This only removes it from this app and the herdr terminal app. Its herdr session keeps running on the machine.",
                "Remove",
                true,
                Command::RemoveProfile(profile_id.to_owned()),
            ),
        ));
        window.focus(&self.form_focus);
    }

    fn remove_profile(&mut self, profile_id: &str) {
        if let Some(index) = self.host_index_for_profile(profile_id) {
            self.remove_host(index);
            return;
        }
        let Ok(id) = ProfileId::parse(profile_id) else {
            return;
        };
        self.catalog.remove_ssh(&id);
        if let Err(error) = self.catalog.store_profiles() {
            self.show_notice(format!("could not save machines: {error}"));
        }
    }

    fn open_menu(&mut self, host: usize, position: Point<Pixels>, items: Vec<MenuItem>) {
        if items.is_empty() || self.host_form.is_some() || self.dialog.is_some() {
            return;
        }
        self.context_menu = Some(ContextMenu {
            host,
            position,
            items,
        });
    }

    pub(crate) fn close_menu(&mut self) {
        self.context_menu = None;
    }

    /// Opens the pane menu for a right click, unless that pane takes right clicks itself.
    fn open_pane_menu(&mut self, position: Point<Pixels>) -> bool {
        if self.modal_open() {
            return false;
        }
        let Some((col, row)) = self.mouse_cell(position) else {
            return false;
        };
        let host = self.selected;
        let Some(surface) = self.hosts[host].surface.clone() else {
            return false;
        };
        let Some(surface_pane) = terminal::pane_at(&surface.panes, col, row) else {
            return false;
        };
        let Some(snapshot) = self.hosts[host].snapshot.as_ref() else {
            return false;
        };
        let Some(pane) = snapshot
            .panes
            .iter()
            .find(|pane| pane.pane_id == surface_pane.pane_id)
        else {
            return false;
        };
        if pane.right_click_passthrough {
            return false;
        }
        let items = menu::pane_menu(pane, snapshot.focused_pane_id.as_deref());
        self.open_menu(host, position, items);
        true
    }

    pub(crate) fn activate_menu_item(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.context_menu.take() else {
            return;
        };
        if let Some(MenuItem::Action { command, .. }) = menu.items.into_iter().nth(index) {
            self.run_command(menu.host, command, window, cx);
        }
    }

    fn run_command(
        &mut self,
        host: usize,
        command: Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if host >= self.hosts.len() {
            return;
        }
        match command {
            Command::Dialog(dialog) => {
                self.dialog = Some((host, *dialog));
                window.focus(&self.form_focus);
                return;
            }
            Command::Reconnect => {
                self.hosts[host].failures = 0;
                self.connect(host);
                return;
            }
            Command::UpdateAndRestart => {
                self.hosts[host].allow_remote_restart = true;
                self.connect(host);
                return;
            }
            Command::RemoveMachine => {
                self.remove_host(host);
                return;
            }
            Command::RemoveProfile(profile_id) => {
                self.remove_profile(&profile_id);
                return;
            }
            Command::TrustHostKey => {
                self.trust_host_key(host, cx);
                return;
            }
            _ => {}
        }
        // Remaining actions change what the machine shows, so bring it forward.
        self.select_host(host);
        match command {
            Command::Requests(requests) => {
                for (method, params) in requests {
                    self.request(method, params);
                }
            }
            Command::NewWorkspace => self.new_workspace(),
            Command::Paste => self.paste(cx),
            _ => {}
        }
        self.restore_focus(window);
    }

    pub(crate) fn on_dialog_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, dialog)) = self.dialog.as_mut() else {
            return;
        };
        let keystroke = &event.keystroke;
        let paste = keystroke.key == "v"
            && if cfg!(target_os = "macos") {
                keystroke.modifiers.platform
            } else {
                keystroke.modifiers.control
            };
        let outcome = if paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                dialog.insert_text(text.lines().next().unwrap_or_default());
            }
            DialogOutcome::Pending
        } else {
            dialog.key(keystroke)
        };
        self.finish_dialog(outcome, window, cx);
        cx.stop_propagation();
        cx.notify();
    }

    pub(crate) fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, dialog)) = self.dialog.as_mut() {
            let outcome = dialog.submit();
            self.finish_dialog(outcome, window, cx);
        }
    }

    pub(crate) fn close_dialog(&mut self, window: &mut Window) {
        self.dialog = None;
        self.restore_focus(window);
    }

    fn finish_dialog(
        &mut self,
        outcome: DialogOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.dialog.as_ref().map(|(host, _)| *host) else {
            return;
        };
        match outcome {
            DialogOutcome::Pending => {}
            DialogOutcome::Cancel => self.close_dialog(window),
            DialogOutcome::Run(command) => {
                self.close_dialog(window);
                self.run_command(host, command, window, cx);
            }
            DialogOutcome::RenameMachine(label) => match self.rename_host(host, label) {
                Ok(()) => self.close_dialog(window),
                Err(error) => {
                    if let Some((_, dialog)) = self.dialog.as_mut() {
                        dialog.error = Some(error);
                    }
                }
            },
        }
    }

    fn rename_host(&mut self, index: usize, label: String) -> Result<(), String> {
        let ClientEndpointId::Ssh(profile_id) = self.hosts[index].id.clone() else {
            return Err("Local can't be renamed".into());
        };
        self.catalog.rename_ssh(&profile_id, label.clone())?;
        self.catalog
            .store_profiles()
            .map_err(|error| format!("could not save machines: {error}"))?;
        self.hosts[index].label = label;
        Ok(())
    }

    pub(crate) fn open_form(&mut self, window: &mut Window) {
        self.host_form = Some(HostForm::default());
        window.focus(&self.form_focus);
    }

    fn close_form(&mut self, window: &mut Window) {
        self.host_form = None;
        self.restore_focus(window);
    }
}

// ----- rendering -------------------------------------------------------------

/// Throttles terminal resizes: at most one per `RESIZE_INTERVAL`, and none while the
/// previous one is unanswered (up to `RESIZE_REPLY_TIMEOUT`). The final size always goes out.
#[derive(Default)]
struct ResizeThrottle {
    pending: bool,
    retry_scheduled: bool,
    sent_at: Option<Instant>,
    awaiting: Option<(u16, u16)>,
}

impl ResizeThrottle {
    /// When the pending size may be sent, or `None` when nothing is pending.
    fn ready_at(&self, now: Instant) -> Option<Instant> {
        if !self.pending {
            return None;
        }
        Some(match self.sent_at {
            None => now,
            Some(sent) if self.awaiting.is_some() => sent + RESIZE_REPLY_TIMEOUT,
            Some(sent) => sent + RESIZE_INTERVAL,
        })
    }

    fn sent(&mut self, now: Instant, size: (u16, u16)) {
        self.pending = false;
        self.sent_at = Some(now);
        self.awaiting = Some(size);
    }

    /// Records a surface of `size`; true when it answers the resize in flight.
    fn answered(&mut self, size: (u16, u16)) -> bool {
        if self.awaiting == Some(size) {
            self.awaiting = None;
            return true;
        }
        false
    }
}

fn is_dark(appearance: gpui::WindowAppearance) -> bool {
    matches!(
        appearance,
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    )
}

fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(8.0)).rounded_full().bg(color).flex_none()
}

fn host_status_color(theme: &Theme, host: &HostState) -> Hsla {
    match &host.status {
        HostStatus::Online => theme.status_color(AgentStatus::Idle),
        HostStatus::Connecting
        | HostStatus::SettingUp(_)
        | HostStatus::NeedsRestart(_)
        | HostStatus::NeedsTrust(_) => theme.status_color(AgentStatus::Working),
        HostStatus::Offline(_) => theme.danger,
    }
}

impl GuiApp {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let mut list = div().flex().flex_col().gap(px(2.0)).p(px(8.0));
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .pt(px(4.0))
                .pb(px(8.0))
                .child(gpui::img(self.logo.clone()).size(px(22.0)).rounded(px(5.0)))
                .child(div().flex_1().text_sm().child("herdr"))
                .child(
                    div()
                        .id("window-grip")
                        .px(px(6.0))
                        .rounded(px(5.0))
                        .text_color(theme.text_muted)
                        .cursor_grab()
                        .hover(|style| style.bg(theme.hover).text_color(theme.text))
                        .on_mouse_down(MouseButton::Left, |_, window, _| {
                            window.start_window_move();
                        })
                        .child("⠿"),
                ),
        );
        let settings_button = div()
            .id("open-settings")
            .px(px(6.0))
            .py(px(2.0))
            .rounded(px(5.0))
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|style| style.bg(theme.hover).text_color(theme.text))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_settings(window, cx);
                cx.notify();
            }))
            .flex_none()
            .text_sm()
            .whitespace_nowrap()
            .child("⚙ Settings");
        list = list.child(section_title("MACHINES", theme));

        for (index, host) in self.hosts.iter().enumerate() {
            let selected = index == self.selected;
            let detail: SharedString = match (&host.status, &host.target) {
                (HostStatus::Offline(error), _) => error.clone().into(),
                (HostStatus::Connecting, _) => "connecting…".into(),
                (HostStatus::SettingUp(message), _) => message.clone().into(),
                (HostStatus::NeedsRestart(message), _) => message.clone().into(),
                (HostStatus::NeedsTrust(message), _) => message.clone().into(),
                (HostStatus::Online, HostTarget::Ssh { target, .. }) => target.clone().into(),
                (HostStatus::Online, HostTarget::Local) => "this computer".into(),
            };
            let mut row = div()
                .id(("host", index))
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .when(selected, |row| row.bg(theme.selected))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.hosts[index].expanded = true;
                    this.select_host(index);
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        let items = menu::machine_menu(&this.hosts[index]);
                        this.open_menu(index, event.position, items);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(dot(host_status_color(theme, host)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(host.label.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .truncate()
                                .child(detail),
                        ),
                );
            let aggregate = host.aggregate_status();
            if aggregate != AgentStatus::Unknown {
                row = row.child(dot(theme.status_color(aggregate)));
            }
            if matches!(host.status, HostStatus::NeedsRestart(_)) {
                row = row.child(
                    div()
                        .id(("restart", index))
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .text_xs()
                        .bg(theme.status_color(AgentStatus::Working))
                        .text_color(theme.panel_bg)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.hosts[index].allow_remote_restart = true;
                            this.connect(index);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("Update & restart"),
                );
            }
            if matches!(host.status, HostStatus::NeedsTrust(_)) {
                row = row.child(
                    div()
                        .id(("trust", index))
                        .px(px(6.0))
                        .py(px(2.0))
                        .rounded(px(4.0))
                        .text_xs()
                        .bg(theme.accent)
                        .text_color(theme.panel_bg)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.open_trust_dialog(index, window);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .child("Trust…"),
                );
            }
            if matches!(host.status, HostStatus::Offline(_)) {
                row = row.child(
                    icon_button(("retry", index), "↻", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.connect(index);
                            cx.stop_propagation();
                            cx.notify();
                        },
                    )),
                );
            }
            if matches!(host.target, HostTarget::Ssh { .. }) {
                row = row.child(
                    icon_button(("remove", index), "×", theme).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.remove_host(index);
                            cx.stop_propagation();
                            cx.notify();
                        },
                    )),
                );
            }
            list = list.child(row);

            if let Some(snapshot) = host.snapshot.as_ref().filter(|_| host.expanded) {
                for (ws_index, workspace) in snapshot.workspaces.iter().enumerate() {
                    let workspace_id = workspace.workspace_id.clone();
                    let focused = selected && workspace.focused;
                    let branch = workspace.branch.clone().unwrap_or_default();
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("ws-{index}-{ws_index}")))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    let items = this.hosts[index]
                                        .snapshot
                                        .as_ref()
                                        .and_then(|s| s.workspaces.get(ws_index))
                                        .map(menu::workspace_menu);
                                    if let Some(items) = items {
                                        this.open_menu(index, event.position, items);
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            )
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .ml(px(16.0))
                            .px(px(8.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .cursor_pointer()
                            .text_sm()
                            .hover(|style| style.bg(theme.hover))
                            .when(focused, |row| row.bg(theme.selected))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.select_host(index);
                                this.request(
                                    "workspace.focus",
                                    serde_json::json!({ "workspace_id": workspace_id }),
                                );
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_color(theme.text_muted)
                                    .child(workspace.number.to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(workspace.label.clone()),
                            )
                            .when(!branch.is_empty(), |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.text_muted)
                                        .max_w(px(90.0))
                                        .truncate()
                                        .child(branch),
                                )
                            })
                            .when(workspace.agent_status != AgentStatus::Unknown, |row| {
                                row.child(dot(theme.status_color(workspace.agent_status)))
                            }),
                    );
                }
            }
        }

        list = list
            .child(div().h(px(12.0)))
            .child(section_title("AGENTS", theme));
        // Only the selected machine's agents, like its workspaces and tabs.
        let mut agents: Vec<(usize, &crate::protocol::ClientShellAgent, String)> = Vec::new();
        for (index, host) in self.hosts.iter().enumerate() {
            if index != self.selected {
                continue;
            }
            let Some(snapshot) = &host.snapshot else {
                continue;
            };
            for agent in &snapshot.agents {
                let workspace = snapshot
                    .workspaces
                    .iter()
                    .find(|w| w.workspace_id == agent.workspace_id)
                    .map(|w| w.label.clone())
                    .unwrap_or_default();
                agents.push((index, agent, workspace));
            }
        }
        agents.sort_by_key(|(_, agent, _)| std::cmp::Reverse(status_priority(agent.agent_status)));
        if agents.is_empty() {
            list = list.child(
                div()
                    .px(px(8.0))
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("No agents running"),
            );
        }
        for (agent_index, (host_index, agent, context)) in agents.into_iter().enumerate() {
            let menu_pane_id = agent.pane_id.clone();
            let name = agent
                .name
                .clone()
                .or_else(|| agent.display_agent.clone())
                .or_else(|| agent.agent.clone())
                .unwrap_or_else(|| "agent".into());
            let status = agent
                .state_labels
                .iter()
                .find(|(state, _)| state == status_label(agent.agent_status))
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| status_label(agent.agent_status).to_owned());
            let pane_id = agent.pane_id.clone();
            list = list.child(
                div()
                    .id(("agent", agent_index))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            let items = this.hosts[host_index].snapshot.as_ref().and_then(|s| {
                                let agent = s.agents.iter().find(|a| a.pane_id == menu_pane_id)?;
                                let pane = s.panes.iter().find(|p| p.pane_id == menu_pane_id);
                                Some(menu::agent_menu(agent, pane))
                            });
                            if let Some(items) = items {
                                this.open_menu(host_index, event.position, items);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(8.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.hover))
                    .when(agent.focused && host_index == self.selected, |row| {
                        row.bg(theme.selected)
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.select_host(host_index);
                        this.request("pane.focus", serde_json::json!({ "pane_id": pane_id }));
                        cx.notify();
                    }))
                    .child(dot(theme.status_color(agent.agent_status)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .justify_between()
                                    .text_sm()
                                    .child(div().truncate().child(name))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.status_color(agent.agent_status))
                                            .child(status),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(context),
                            ),
                    ),
            );
        }

        if let Some(stats) = self.machine_stats.get(&self.selected_host().id) {
            list = list
                .child(div().h(px(12.0)))
                .child(section_title("MACHINE", theme))
                .child(machine_stats_view(stats, theme));
        }

        if self.prefs.usage_enabled {
            list = list
                .child(div().h(px(12.0)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(div().flex_1().child(section_title("USAGE", theme)))
                        .child(icon_button(("usage-refresh", 0), "↻", theme).on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                if !this.refresh_usage(this.selected, cx) {
                                    this.show_notice("This machine isn't connected yet".into());
                                }
                                cx.notify();
                            }),
                        )),
                )
                .child(usage_view::render(
                    self.usage.get(&self.selected_host().id),
                    self.claude_limits.get(&self.selected_host().id),
                    &self.usage_expanded,
                    theme,
                    cx,
                ));
            if let HostTarget::Ssh { .. } = self.selected_host().target {
                if !self.prefs.remote_usage {
                    list = list.child(
                        div()
                            .px(px(8.0))
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Usage for SSH machines is off in Settings"),
                    );
                }
            }
        }

        let resize_handle = div()
            .id("sidebar-resize")
            .absolute()
            .top_0()
            .right(px(-3.0))
            .w(px(6.0))
            .h_full()
            .cursor_col_resize()
            .hover(|style| style.bg(theme.border))
            .when(self.sidebar_dragging, |handle| handle.bg(theme.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.sidebar_dragging = true;
                    cx.stop_propagation();
                    cx.notify();
                }),
            );

        div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(self.prefs.sidebar_width))
            .h_full()
            .bg(theme.sidebar_bg)
            .border_r_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .p(px(8.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(settings_button),
            )
            .child(resize_handle)
    }

    fn drag_sidebar(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.sidebar_dragging {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.finish_sidebar_drag(cx);
            return;
        }
        self.prefs.sidebar_width =
            f32::from(event.position.x).clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
        cx.notify();
    }

    fn finish_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        if !self.sidebar_dragging {
            return;
        }
        self.sidebar_dragging = false;
        if let Err(error) = self.prefs.store() {
            warn!(%error, "could not save the sidebar width");
        }
        cx.notify();
    }

    fn render_tab_bar(&self, show_shortcuts: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .h(px(36.0))
            .px(px(8.0))
            .overflow_hidden()
            .bg(theme.panel_bg)
            .border_b_1()
            .border_color(theme.border);
        // Tabs shrink and clip before the toolbar does.
        let mut tabs = div()
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(4.0))
            .px(px(3.0))
            .overflow_hidden();
        if let Some(snapshot) = &self.selected_host().snapshot {
            let workspace_id = snapshot.focused_workspace_id.clone();
            let tab_count = snapshot
                .tabs
                .iter()
                .filter(|tab| Some(&tab.workspace_id) == workspace_id.as_ref())
                .count();
            for (index, tab) in snapshot
                .tabs
                .iter()
                .filter(|tab| Some(&tab.workspace_id) == workspace_id.as_ref())
                .enumerate()
            {
                let landing = match &self.drag_target {
                    Some(DragTarget::Tab { from, onto, after })
                        if *onto == index && tab_insert_index(*from, *onto, *after).is_some() =>
                    {
                        Some(*after)
                    }
                    _ => None,
                };
                let lifted = matches!(&self.drag_target, Some(DragTarget::Tab { from, .. }) if *from == index);
                let preview_label = tab.label.clone();
                let preview_theme = theme.clone();
                let grip = div()
                    .id(("tab-grip", index))
                    .flex_none()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .cursor_grab()
                    .on_drag(
                        DraggedTab {
                            tab_id: tab.tab_id.clone(),
                            index,
                        },
                        move |_, _, _, cx| drag_preview(preview_label.clone(), &preview_theme, cx),
                    )
                    .child("⋮⋮");
                let shortcut = show_shortcuts
                    .then(|| tab_shortcut_label(index, tab_count))
                    .flatten();
                let tab_id = tab.tab_id.clone();
                let close_id = tab.tab_id.clone();
                let menu_tab_id = tab.tab_id.clone();
                tabs = tabs.child(
                    div()
                        .id(("tab", index))
                        .relative()
                        .flex_shrink_0()
                        .max_w(px(220.0))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                let host = this.selected;
                                let items = this.hosts[host].snapshot.as_ref().and_then(|s| {
                                    s.tabs
                                        .iter()
                                        .find(|tab| tab.tab_id == menu_tab_id)
                                        .map(menu::tab_menu)
                                });
                                if let Some(items) = items {
                                    this.open_menu(host, event.position, items);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(10.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .text_sm()
                        .hover(|style| style.bg(theme.hover))
                        .when(tab.focused, |tab| tab.bg(theme.selected))
                        .when(!tab.focused, |tab| tab.text_color(theme.text_muted))
                        .when(lifted, |tab| tab.opacity(0.35))
                        .on_drag_move(cx.listener(
                            move |this, event: &DragMoveEvent<DraggedTab>, _, cx| {
                                let position = event.event.position;
                                if !event.bounds.contains(&position) {
                                    return;
                                }
                                let target = DragTarget::Tab {
                                    from: event.drag(cx).index,
                                    onto: index,
                                    after: position.x > event.bounds.center().x,
                                };
                                this.set_drag_target(Some(target), cx);
                            },
                        ))
                        .when_some(landing, |tab, after| {
                            tab.child(
                                div()
                                    .absolute()
                                    .top(px(2.0))
                                    .bottom(px(2.0))
                                    .w(px(2.0))
                                    .rounded_full()
                                    .bg(theme.accent)
                                    .when(after, |bar| bar.right(px(-3.0)))
                                    .when(!after, |bar| bar.left(px(-3.0)))
                                    .with_animation(
                                        SharedString::from(format!("tab-landing-{index}-{after}")),
                                        landing_fade(),
                                        |bar, t| bar.opacity(t),
                                    ),
                            )
                        })
                        .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                            this.drop_tab(dragged, index);
                            cx.notify();
                        }))
                        .child(grip)
                        .when_some(shortcut, |tab, shortcut| {
                            tab.child(
                                div()
                                    .flex_none()
                                    .px(px(4.0))
                                    .rounded(px(4.0))
                                    .text_xs()
                                    .bg(theme.border)
                                    .text_color(theme.text)
                                    .child(shortcut),
                            )
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.request("tab.focus", serde_json::json!({ "tab_id": tab_id }));
                            cx.notify();
                        }))
                        .when(tab.agent_status != AgentStatus::Unknown, |row| {
                            row.child(dot(theme.status_color(tab.agent_status)))
                        })
                        .child(div().min_w_0().truncate().child(tab.label.clone()))
                        .child(icon_button(("tab-close", index), "×", theme).on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.request(
                                    "tab.close",
                                    serde_json::json!({ "tab_id": close_id }),
                                );
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )),
                );
            }
        }
        tabs = tabs.child(toolbar_button("new-tab", "+", theme).on_click(cx.listener(
            |this, _: &ClickEvent, _, cx| {
                this.new_tab();
                cx.notify();
            },
        )));
        bar.child(tabs)
            .child(
                toolbar_button("split-right", "Split ┃", theme).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.split("right");
                        cx.notify();
                    },
                )),
            )
            .child(
                toolbar_button("split-down", "Split ━", theme).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.split("down");
                        cx.notify();
                    },
                )),
            )
            .child(
                toolbar_button("new-workspace", "New workspace", theme).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.new_workspace();
                        cx.notify();
                    },
                )),
            )
    }

    fn render_terminal(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let host = self.selected_host();
        let surface = host.surface.clone();
        let placeholder: Option<SharedString> = match (&host.status, &surface) {
            (_, Some(_)) => None,
            (HostStatus::Connecting, None) => Some(format!("Connecting to {}…", host.label).into()),
            (HostStatus::SettingUp(message), None) => {
                Some(format!("{}: {message}", host.label).into())
            }
            (HostStatus::NeedsRestart(message), None) => Some(
                format!(
                    "{message}. Click 'Update & restart' next to {} to continue.",
                    host.label
                )
                .into(),
            ),
            (HostStatus::NeedsTrust(_), None) => Some(
                format!(
                    "This is the first connection to {}. Click 'Trust…' next to it to check its host key.",
                    host.label
                )
                .into(),
            ),
            (HostStatus::Offline(error), None) => {
                Some(format!("{} is offline: {error}", host.label).into())
            }
            (HostStatus::Online, None) => Some("Waiting for the terminal…".into()),
        };
        let metrics = self.metrics.clone();
        let theme = self.theme.clone();
        let origin = self.terminal_origin.clone();
        let grid = self.grid;
        let selection = self.selection.clone();
        let entity = cx.entity().downgrade();
        let window_active = window.is_window_active();

        // While a pane is dragged: the source looks lifted, the target is outlined.
        let mut pane_overlays: Vec<AnyElement> = Vec::new();
        if let (Some(DragTarget::Pane { source, target }), Some(surface), Some(metrics)) =
            (&self.drag_target, &surface, &metrics)
        {
            for pane in &surface.panes {
                let is_target = target.as_ref() == Some(&pane.pane_id);
                if pane.pane_id != *source && !is_target {
                    continue;
                }
                let overlay = div()
                    .absolute()
                    .left(metrics.cell_width * f32::from(pane.rect.x))
                    .top(metrics.line_height * f32::from(pane.rect.y))
                    .w(metrics.cell_width * f32::from(pane.rect.width))
                    .h(metrics.line_height * f32::from(pane.rect.height));
                pane_overlays.push(if is_target {
                    overlay
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .border_2()
                        .border_color(theme.accent)
                        .bg(Hsla {
                            a: 0.12,
                            ..theme.accent
                        })
                        .child(
                            div()
                                .px(px(10.0))
                                .py(px(4.0))
                                .rounded(px(6.0))
                                .text_sm()
                                .bg(theme.panel_bg)
                                .text_color(theme.text)
                                .border_1()
                                .border_color(theme.border)
                                .child("Swap here"),
                        )
                        .with_animation(
                            SharedString::from(format!("pane-landing-{}", pane.pane_id)),
                            landing_fade(),
                            |overlay, t| overlay.opacity(t),
                        )
                        .into_any_element()
                } else {
                    overlay
                        .bg(Hsla {
                            a: 0.45,
                            ..theme.terminal_bg
                        })
                        .into_any_element()
                });
            }
        }

        let mut pane_grips = Vec::new();
        if let (Some(surface), Some(metrics)) = (&surface, &metrics) {
            if surface.panes.len() > 1 {
                for pane in &surface.panes {
                    let bounds = pane_grip_bounds(pane, metrics);
                    let preview_theme = theme.clone();
                    pane_grips.push(
                        div()
                            .id(SharedString::from(format!("pane-grip-{}", pane.pane_id)))
                            .absolute()
                            .left(bounds.origin.x)
                            .top(bounds.origin.y)
                            .w(bounds.size.width)
                            .h(bounds.size.height)
                            .rounded_full()
                            .bg(theme.border)
                            .hover(|style| style.bg(theme.text_muted))
                            .cursor_grab()
                            .on_drag(
                                DraggedPane {
                                    pane_id: pane.pane_id.clone(),
                                },
                                move |_, _, _, cx| drag_preview("Move pane", &preview_theme, cx),
                            ),
                    );
                }
            }
        }

        let surface_canvas = canvas(
            {
                let metrics = metrics.clone();
                move |bounds, _, cx| {
                    origin.set(bounds.origin);
                    if let Some(metrics) = &metrics {
                        let size = metrics.grid_size(bounds);
                        if size != grid {
                            cx.defer(move |cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    this.resize_terminal(size.0, size.1, cx)
                                });
                            });
                        }
                    }
                }
            },
            move |bounds, (), window, cx| {
                window.paint_quad(gpui::fill(bounds, theme.terminal_bg));
                if let (Some(surface), Some(metrics)) = (surface, metrics) {
                    // A selection whose pane is no longer shown isn't drawn.
                    let selection = selection.as_ref().filter(|selection| {
                        surface
                            .panes
                            .iter()
                            .any(|pane| pane.pane_id == selection.pane_id)
                    });
                    terminal::paint_frame(
                        bounds,
                        &surface.frame,
                        &surface.panes,
                        selection,
                        &metrics,
                        &theme,
                        window_active,
                        window,
                        cx,
                    );
                }
            },
        )
        .size_full();

        div()
            .id("terminal")
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&self.terminal_focus)
            .on_key_down(cx.listener(Self::on_terminal_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    if this.modal_open() || this.on_pane_grip(event.position) {
                        return;
                    }
                    window.focus(&this.terminal_focus);
                    // Cmd+click (Ctrl+click elsewhere) opens a link, as in other terminals.
                    let link_modifier = if cfg!(target_os = "macos") {
                        event.modifiers.platform
                    } else {
                        event.modifiers.control
                    };
                    if link_modifier {
                        if let Some(url) = this.link_at(event.position) {
                            cx.open_url(&url);
                            return;
                        }
                    }
                    let had_selection = this.selection.is_some();
                    if this.start_selection(event.position, &event.modifiers) || had_selection {
                        cx.notify();
                    }
                    if this.selecting {
                        return;
                    }
                    this.on_terminal_mouse(
                        ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
                        event.position,
                        &event.modifiers,
                    );
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    if this.open_pane_menu(event.position) {
                        cx.notify();
                        return;
                    }
                    this.on_terminal_mouse(
                        ClientMouseKind::Down(crate::protocol::ClientMouseButton::Right),
                        event.position,
                        &event.modifiers,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.on_terminal_mouse(
                        ClientMouseKind::Down(crate::protocol::ClientMouseButton::Middle),
                        event.position,
                        &event.modifiers,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if this.selecting {
                        this.finish_selection(cx);
                        return;
                    }
                    this.on_terminal_mouse(
                        ClientMouseKind::Up(crate::protocol::ClientMouseButton::Left),
                        event.position,
                        &event.modifiers,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, _, _| {
                    this.on_terminal_mouse(
                        ClientMouseKind::Up(crate::protocol::ClientMouseButton::Right),
                        event.position,
                        &event.modifiers,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, _, _| {
                    this.on_terminal_mouse(
                        ClientMouseKind::Up(crate::protocol::ClientMouseButton::Middle),
                        event.position,
                        &event.modifiers,
                    );
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if cx.has_active_drag() {
                    return;
                }
                if this.selecting {
                    this.extend_selection(event.position);
                    cx.notify();
                    return;
                }
                let Some(button) = event.pressed_button.and_then(terminal::mouse_button) else {
                    return;
                };
                if this.mouse_pane.is_some() {
                    this.on_terminal_mouse(
                        ClientMouseKind::Drag(button),
                        event.position,
                        &event.modifiers,
                    );
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, _| {
                this.on_terminal_scroll(event);
            }))
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedPane>, _, cx| {
                    let source = event.drag(cx).pane_id.clone();
                    let target = this
                        .pane_id_at(event.event.position)
                        .filter(|target| *target != source);
                    this.set_drag_target(Some(DragTarget::Pane { source, target }), cx);
                }),
            )
            .on_drop(cx.listener(|this, dragged: &DraggedPane, window, cx| {
                this.drop_pane(dragged, window.mouse_position());
                cx.notify();
            }))
            .child(surface_canvas)
            .children(pane_overlays)
            .children(pane_grips)
            .when_some(placeholder, |terminal, message| {
                terminal.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(32.0))
                        .text_center()
                        .text_color(self.theme.text_muted)
                        .child(message),
                )
            })
    }
}

fn machine_stats_view(stats: &Result<MachineStats, String>, theme: &Theme) -> gpui::Div {
    let row = |label: &'static str, value: String, percent: Option<f64>| {
        let mut row = div().flex().flex_col().gap(px(2.0)).px(px(8.0)).child(
            div()
                .flex()
                .text_xs()
                .child(div().flex_1().text_color(theme.text_muted).child(label))
                .child(value),
        );
        if let Some(percent) = percent {
            row = row.child(usage_view::bar(theme, percent));
        }
        row
    };
    let stats = match stats {
        Ok(stats) => stats,
        Err(error) => {
            return div()
                .px(px(8.0))
                .text_xs()
                .text_color(theme.text_muted)
                .child(error.clone());
        }
    };
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(row(
            "CPU",
            format!("{:.0}%", stats.cpu_percent),
            Some(stats.cpu_percent),
        ))
        .child(row(
            "Memory",
            format!(
                "{} / {}",
                machine_stats::format_bytes(stats.mem_used),
                machine_stats::format_bytes(stats.mem_total)
            ),
            Some(stats.mem_percent()),
        ))
        .child(row(
            "Network",
            format!(
                "↓ {}/s  ↑ {}/s",
                machine_stats::format_bytes(stats.net_rx_per_sec),
                machine_stats::format_bytes(stats.net_tx_per_sec)
            ),
            None,
        ))
}

fn section_title(title: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .px(px(8.0))
        .pt(px(6.0))
        .pb(px(4.0))
        .text_xs()
        .text_color(theme.text_muted)
        .child(title)
}

fn icon_button(
    id: (&'static str, usize),
    label: &'static str,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(4.0))
        .rounded(px(4.0))
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(theme.border))
        .child(label)
}

fn toolbar_button(
    id: &'static str,
    label: &'static str,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .whitespace_nowrap()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .text_sm()
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover).text_color(theme.text))
        .child(label)
}

impl Render for GuiApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.metrics.is_none() {
            self.metrics = Some(CellMetrics::measure(
                window,
                gpui::font(terminal::default_font_family()),
                px(self.prefs.font_size),
            ));
        }
        let theme = self.theme.clone();
        let notice: Option<SharedString> = self.notice.as_ref().map(|(n, _)| n.clone().into());
        let form: Option<AnyElement> = self
            .host_form
            .as_ref()
            .map(|form| form.render(&theme, &self.form_focus, cx).into_any_element());
        let settings_page: Option<AnyElement> = self.settings.map(|section| {
            settings::render(
                SettingsView {
                    section,
                    machines: self.machine_rows(),
                    limits: self.limits_rows(),
                    prefs: &self.prefs,
                    update: self.update_view(),
                    max_fps: self.local_max_fps,
                    theme_name: &self.theme_name,
                    theme_auto: self.theme_auto,
                    dark: self.dark_appearance,
                },
                &self.settings_focus,
                &theme,
                cx,
            )
            .into_any_element()
        });
        let dialog: Option<AnyElement> = self.dialog.as_ref().map(|(_, dialog)| {
            dialog
                .render(&theme, &self.form_focus, cx)
                .into_any_element()
        });
        let viewport = window.viewport_size();
        let context_menu: Option<AnyElement> = self
            .context_menu
            .as_ref()
            .map(|open| menu::render(open, viewport, &theme, cx).into_any_element());

        div()
            .relative()
            .flex()
            .size_full()
            .bg(theme.terminal_bg)
            .text_color(theme.text)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_sidebar(event, cx);
            }))
            // Tab shortcut hints show while the app modifier is held.
            .on_modifiers_changed(cx.listener(|_, _: &ModifiersChangedEvent, _, cx| cx.notify()))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    this.finish_sidebar_drag(cx);
                    this.set_drag_target(None, cx);
                }),
            )
            .when(self.sidebar_dragging, |root| root.cursor_col_resize())
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .children(self.render_update_banner(cx))
                    .child(self.render_tab_bar(app_chord_held(&window.modifiers()), cx))
                    .child(self.render_terminal(window, cx))
                    .when_some(notice, |column, notice| {
                        column.child(
                            div()
                                .flex_none()
                                .px(px(12.0))
                                .py(px(4.0))
                                .text_xs()
                                .bg(theme.panel_bg)
                                .text_color(theme.text_muted)
                                .border_t_1()
                                .border_color(theme.border)
                                .truncate()
                                .child(notice),
                        )
                    }),
            )
            .when_some(settings_page, |root, page| root.child(page))
            .when_some(form, |root, form| root.child(form))
            .when_some(dialog, |root, dialog| root.child(dialog))
            .when_some(context_menu, |root, menu| root.child(menu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_shortcuts_cover_the_first_eight_and_the_last_tab() {
        let digit = |index, count| tab_shortcut_label(index, count).and_then(|l| l.chars().last());
        assert_eq!(digit(0, 3), Some('1'));
        assert_eq!(digit(7, 12), Some('8'));
        assert_eq!(digit(8, 12), None);
        assert_eq!(digit(11, 12), Some('9'));
    }

    #[test]
    fn dropping_a_tab_lands_on_the_target_position() {
        // Either edge of the dragged tab itself, or the gap next to it, is a no-op.
        assert_eq!(tab_insert_index(2, 2, false), None);
        assert_eq!(tab_insert_index(2, 2, true), None);
        assert_eq!(tab_insert_index(2, 1, true), None);
        assert_eq!(tab_insert_index(2, 3, false), None);
        // Tab 0 dropped on the right half of tab 2 lands after it.
        assert_eq!(tab_insert_index(0, 2, true), Some(3));
        assert_eq!(tab_insert_index(0, 2, false), Some(2));
        assert_eq!(tab_insert_index(3, 1, false), Some(1));
    }

    #[test]
    fn resizes_wait_for_the_previous_answer_and_the_interval() {
        let start = Instant::now();
        let mut throttle = ResizeThrottle::default();
        assert_eq!(throttle.ready_at(start), None);

        throttle.pending = true;
        assert_eq!(throttle.ready_at(start), Some(start));
        throttle.sent(start, (100, 40));

        // A newer size waits for the server's answer, up to the timeout.
        throttle.pending = true;
        assert_eq!(throttle.ready_at(start), Some(start + RESIZE_REPLY_TIMEOUT));

        // Only an answer at the requested size counts.
        assert!(!throttle.answered((90, 40)));
        assert!(throttle.answered((100, 40)));
        assert_eq!(throttle.ready_at(start), Some(start + RESIZE_INTERVAL));
    }
}
