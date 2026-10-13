//! Background connection to one endpoint (Local or a saved SSH machine).
//!
//! The GUI speaks the same stable endpoint protocol (generation 1) as the TUI
//! client: the server renders pane cells and the GUI only draws them. Each
//! connection runs on its own threads and reports back through one channel, so a
//! slow or unreachable machine never blocks the window or the other machines.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use interprocess::local_socket::traits::Stream as _;
use tracing::warn;

use crate::client::endpoint::{ClientEndpointId, EndpointNegotiation, NativeEndpointTransport};
use crate::protocol::{self, ClientSurfaceSize, ServerMessage};

/// Where a host connection goes.
#[derive(Clone, Debug)]
pub(crate) enum HostTarget {
    Local,
    Ssh {
        profile_id: String,
        target: String,
        session: String,
    },
}

/// Geometry announced in the endpoint hello.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnectGeometry {
    pub(crate) size: ClientSurfaceSize,
    pub(crate) cell_width_px: u32,
    pub(crate) cell_height_px: u32,
}

pub(crate) enum ConnectionEvent {
    /// Long-running setup such as installing herdr on a remote machine.
    Progress(String),
    /// Setup must stop a running remote server (ending its panes) and needs consent.
    RestartRequired(String),
    Connected {
        transport: NativeEndpointTransport,
        negotiation: EndpointNegotiation,
    },
    Message(Box<ServerMessage>),
    Failed(String),
    Disconnected,
}

/// One event tagged with the endpoint and connection attempt that produced it.
pub(crate) struct HostEvent {
    pub(crate) host: ClientEndpointId,
    pub(crate) generation: u64,
    pub(crate) event: ConnectionEvent,
}

/// Stops the reader thread of one connection attempt when dropped.
pub(crate) struct ConnectionGuard {
    stopped: Arc<AtomicBool>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

/// Starts connecting in the background. Events arrive on `events` until the
/// returned guard is dropped or the connection ends.
pub(crate) fn spawn_connection(
    host: ClientEndpointId,
    target: HostTarget,
    generation: u64,
    geometry: ConnectGeometry,
    surface_active: bool,
    allow_remote_restart: bool,
    events: UnboundedSender<HostEvent>,
) -> ConnectionGuard {
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = stopped.clone();
    let spawned = std::thread::Builder::new()
        .name("gui-endpoint-connect".into())
        .spawn({
            let host = host.clone();
            let events = events.clone();
            move || {
                let send = |event| {
                    let _ = events.unbounded_send(HostEvent {
                        host: host.clone(),
                        generation,
                        event,
                    });
                };
                match open(
                    &target,
                    geometry,
                    surface_active,
                    allow_remote_restart,
                    &send,
                ) {
                    Ok(opened) => run_reader(opened, &thread_stopped, send),
                    Err(error) => send(match restart_required(&error) {
                        Some(message) => ConnectionEvent::RestartRequired(message),
                        None => ConnectionEvent::Failed(error.to_string()),
                    }),
                }
            }
        });
    if let Err(error) = spawned {
        let _ = events.unbounded_send(HostEvent {
            host,
            generation,
            event: ConnectionEvent::Failed(format!("could not start connection thread: {error}")),
        });
    }
    ConnectionGuard { stopped }
}

struct OpenedConnection {
    stream: crate::ipc::LocalStream,
    transport: NativeEndpointTransport,
    negotiation: EndpointNegotiation,
}

fn restart_required(error: &io::Error) -> Option<String> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<crate::remote::RemoteServerStopRequired>())
        .map(|required| required.message.clone())
}

/// Connects to a saved machine, first installing or updating herdr there when
/// the existing install cannot serve this client. Adding a machine is the
/// user's consent to install; stopping a running server needs its own consent.
fn connect_ssh(
    profile_id: &str,
    target: &str,
    session: &str,
    allow_remote_restart: bool,
    send: &impl Fn(ConnectionEvent),
) -> io::Result<crate::remote::SavedSshStream> {
    match crate::remote::connect_saved_ssh(profile_id, target, session) {
        Ok(connected) => return Ok(connected),
        Err(error) => {
            tracing::info!(%error, target, "saved machine is not ready; running remote setup");
        }
    }
    send(ConnectionEvent::Progress(format!(
        "checking herdr on {target}; installing or updating it if needed…"
    )));
    crate::remote::prepare_saved_ssh(
        profile_id,
        target,
        session,
        crate::remote::RemoteSetupConsent {
            install: true,
            stop_running_server: allow_remote_restart,
        },
    )?;
    send(ConnectionEvent::Progress(format!(
        "connecting to {target}…"
    )));
    crate::remote::connect_saved_ssh(profile_id, target, session)
}

fn open(
    target: &HostTarget,
    geometry: ConnectGeometry,
    surface_active: bool,
    allow_remote_restart: bool,
    send: &impl Fn(ConnectionEvent),
) -> io::Result<OpenedConnection> {
    use interprocess::TryClone as _;

    let (mut stream, lifetime, local_transport): (_, Box<dyn Send>, bool) = match target {
        HostTarget::Local => {
            crate::server::autodetect::ensure_local_server()?;
            let path = crate::server::socket_paths::client_socket_path();
            (crate::ipc::connect_local_stream(&path)?, Box::new(()), true)
        }
        HostTarget::Ssh {
            profile_id,
            target,
            session,
        } => {
            let connected = connect_ssh(profile_id, target, session, allow_remote_restart, send)?;
            (connected.stream, Box::new(connected.bridge), false)
        }
    };

    let handshake = crate::client::do_handshake(
        &mut stream,
        geometry.size.cols,
        geometry.size.rows,
        geometry.cell_width_px,
        geometry.cell_height_px,
        crate::client::HandshakePixelCapabilities {
            exact_geometry: false,
            sgr_mouse: false,
        },
        Some(geometry.size),
        false,
        true,
        surface_active,
        local_transport,
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    let negotiation = EndpointNegotiation::new(
        handshake.endpoint_methods.unwrap_or_default(),
        handshake.endpoint_capabilities.unwrap_or_default(),
    );

    let reader = stream.try_clone()?;
    let transport = NativeEndpointTransport::with_lifetime(stream, lifetime)?;
    Ok(OpenedConnection {
        stream: reader,
        transport,
        negotiation,
    })
}

fn run_reader(opened: OpenedConnection, stopped: &AtomicBool, send: impl Fn(ConnectionEvent)) {
    let OpenedConnection {
        mut stream,
        transport,
        negotiation,
    } = opened;
    let mut decoder = crate::client::negotiated_surface_decoder(&negotiation);
    let writer_stopped = transport.stop_handle();
    send(ConnectionEvent::Connected {
        transport,
        negotiation,
    });

    if let Err(error) = stream.set_nonblocking(true) {
        warn!(%error, "gui endpoint reader could not enter nonblocking mode");
        send(ConnectionEvent::Disconnected);
        return;
    }
    let mut reader = crate::client::EndpointReader {
        stream: &mut stream,
        stopped,
    };
    loop {
        if stopped.load(Ordering::Acquire) || writer_stopped.load(Ordering::Acquire) {
            break;
        }
        let message = protocol::read_message(&mut reader, protocol::MAX_GRAPHICS_FRAME_SIZE)
            .and_then(|message| match &mut decoder {
                Some(decoder) => decoder.decode(message).map_err(|error| {
                    protocol::FramingError::Io(io::Error::new(io::ErrorKind::InvalidData, error))
                }),
                None => Ok(message),
            });
        match message {
            Ok(message) => send(ConnectionEvent::Message(Box::new(message))),
            Err(protocol::FramingError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(protocol::FramingError::UnexpectedEof) => break,
            Err(error) => {
                warn!(%error, "gui endpoint read failed");
                break;
            }
        }
    }
    if !stopped.load(Ordering::Acquire) {
        send(ConnectionEvent::Disconnected);
    }
}
