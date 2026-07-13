use std::{
    fs,
    future::Future,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use nix::{sys::socket, unistd::Uid};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Mutex,
    task::JoinSet,
};
use waldo_controller::Controller;
use waldo_niri::{NiriInspector, SessionInspector};
use waldo_protocol::{
    Capabilities, ClientMessage, ProtocolError, SCHEMA_MAJOR_VERSION, ServerMessage,
};
use waldo_screencopy::{ScreenCapturer, Screencopy, UnavailableCapturer};
use waldo_socket::{TransportError, read_message, write_message};

use crate::{ServiceError, dispatch::dispatch, socket_path};

pub(crate) struct ServiceState {
    pub controller: Mutex<Controller>,
    pub capabilities: Capabilities,
    pub inspector: Arc<dyn SessionInspector>,
    pub capturer: Arc<dyn ScreenCapturer>,
    started_at: Instant,
}

impl ServiceState {
    fn new(
        idle_timeout: Duration,
        inspector: Arc<dyn SessionInspector>,
        capturer: Arc<dyn ScreenCapturer>,
    ) -> Self {
        let mut capabilities = inspector
            .inspect(false)
            .map(|snapshot| snapshot.capabilities)
            .unwrap_or_else(|_| Capabilities::unavailable());
        enable_capture_capability(&mut capabilities, capturer.as_ref());
        Self {
            controller: Mutex::new(Controller::new(idle_timeout)),
            capabilities,
            inspector,
            capturer,
            started_at: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }
}

/// A bound controller service ready to accept authenticated local clients.
pub struct ControllerService {
    listener: UnixListener,
    state: Arc<ServiceState>,
    _socket_guard: socket_path::SocketGuard,
}

impl ControllerService {
    /// Binds a private socket after rejecting symlink and ownership hazards.
    pub fn bind(path: impl AsRef<Path>, idle_timeout: Duration) -> Result<Self, ServiceError> {
        Self::bind_with_backends(
            path,
            idle_timeout,
            Arc::new(NiriInspector),
            Arc::new(Screencopy),
        )
    }

    /// Binds with an explicit read-only inspector for tests and diagnostics.
    pub fn bind_with_inspector(
        path: impl AsRef<Path>,
        idle_timeout: Duration,
        inspector: Arc<dyn SessionInspector>,
    ) -> Result<Self, ServiceError> {
        Self::bind_with_backends(path, idle_timeout, inspector, Arc::new(UnavailableCapturer))
    }

    /// Binds with explicit read-only backends for deterministic integration tests.
    pub fn bind_with_backends(
        path: impl AsRef<Path>,
        idle_timeout: Duration,
        inspector: Arc<dyn SessionInspector>,
        capturer: Arc<dyn ScreenCapturer>,
    ) -> Result<Self, ServiceError> {
        let path = path.as_ref();
        socket_path::prepare(path)?;

        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            listener,
            state: Arc::new(ServiceState::new(idle_timeout, inspector, capturer)),
            _socket_guard: socket_path::SocketGuard::new(path.to_path_buf())?,
        })
    }

    /// Accepts clients until the supplied shutdown future completes.
    pub async fn run_until<F>(self, shutdown: F) -> Result<(), ServiceError>
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(shutdown);
        let mut connections = JoinSet::new();

        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    let state = Arc::clone(&self.state);
                    connections.spawn(async move {
                        if let Err(error) = handle_connection(stream, state).await {
                            eprintln!("Waldo controller connection ended: {error}");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        eprintln!("Waldo controller connection task failed: {error}");
                    }
                }
            }
        }

        connections.abort_all();
        while connections.join_next().await.is_some() {}
        Ok(())
    }
}

pub(crate) fn enable_capture_capability(
    capabilities: &mut Capabilities,
    capturer: &dyn ScreenCapturer,
) {
    if capturer.available() && capabilities.screencopy.version.is_some() {
        capabilities.screencopy.available = true;
        capabilities.screencopy.limitation = None;
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    state: Arc<ServiceState>,
) -> Result<(), ServiceError> {
    authenticate_peer(&stream)?;

    let first: ClientMessage = read_message(&mut stream).await?;
    let (request_id, client_id) = match first {
        ClientMessage::Hello {
            request_id,
            schema_major,
            client_id,
        } if schema_major == SCHEMA_MAJOR_VERSION => (request_id, client_id),
        ClientMessage::Hello {
            request_id,
            schema_major,
            ..
        } => {
            write_message(
                &mut stream,
                &ServerMessage::Error {
                    request_id: Some(request_id),
                    error: ProtocolError::incompatible_schema(SCHEMA_MAJOR_VERSION, schema_major),
                },
            )
            .await?;
            return Ok(());
        }
        other => {
            write_message(
                &mut stream,
                &ServerMessage::Error {
                    request_id: Some(other.request_id()),
                    error: crate::dispatch::invalid_request("the first message must be hello"),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let connection_id = state.controller.lock().await.connect(client_id);
    write_message(
        &mut stream,
        &ServerMessage::Connected {
            request_id,
            connection_id,
            capabilities: state.capabilities.clone(),
        },
    )
    .await?;

    let result = connection_loop(&mut stream, &state, connection_id).await;
    let _ = state.controller.lock().await.disconnect(connection_id);
    result
}

async fn connection_loop(
    stream: &mut UnixStream,
    state: &ServiceState,
    connection_id: waldo_protocol::ConnectionId,
) -> Result<(), ServiceError> {
    loop {
        let message: ClientMessage = match read_message(stream).await {
            Ok(message) => message,
            Err(TransportError::ConnectionClosed) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let response = dispatch(message, state, connection_id).await;
        write_message(stream, &response).await?;
    }
}

fn authenticate_peer(stream: &UnixStream) -> Result<(), ServiceError> {
    let credentials = socket::getsockopt(stream, socket::sockopt::PeerCredentials)
        .map_err(|error| ServiceError::PeerCredential(error.to_string()))?;
    let expected = Uid::effective().as_raw();
    if credentials.uid() != expected {
        return Err(ServiceError::PeerCredential(format!(
            "peer uid {} does not match controller uid {expected}",
            credentials.uid()
        )));
    }
    Ok(())
}
