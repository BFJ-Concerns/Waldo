use std::{
    collections::VecDeque,
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
    sync::{Mutex, Notify, watch},
    task::JoinSet,
};
use waldo_accessibility::AccessibilityBackend;
use waldo_action::{ActionExecutor, UnavailableActionExecutor};
use waldo_controller::Controller;
use waldo_niri::{NiriInspector, SessionInspector};
use waldo_protocol::{
    Capabilities, ClientMessage, ProtocolError, SCHEMA_MAJOR_VERSION, ServerMessage,
};
use waldo_screencopy::{ScreenCapturer, Screencopy, UnavailableCapturer};
use waldo_socket::{TransportError, read_message, write_message};

use crate::{ServiceError, dispatch::dispatch, executor::DesktopActionExecutor, socket_path};

pub(crate) struct ServiceState {
    pub controller: Mutex<Controller>,
    pub capabilities: Capabilities,
    pub inspector: Arc<dyn SessionInspector>,
    pub capturer: Arc<dyn ScreenCapturer>,
    pub accessibility: Option<Arc<AccessibilityBackend>>,
    pub executor: Arc<dyn ActionExecutor>,
    execution_quiesced: Notify,
    shutdown: watch::Sender<bool>,
    started_at: Instant,
}

impl ServiceState {
    fn new(
        idle_timeout: Duration,
        inspector: Arc<dyn SessionInspector>,
        capturer: Arc<dyn ScreenCapturer>,
        accessibility: Option<Arc<AccessibilityBackend>>,
        accessibility_failure: Option<String>,
        executor: Arc<dyn ActionExecutor>,
    ) -> Self {
        let mut capabilities = inspector
            .inspect(false)
            .map(|snapshot| snapshot.capabilities)
            .unwrap_or_else(|_| Capabilities::unavailable());
        enable_capture_capability(&mut capabilities, capturer.as_ref());
        enable_action_capabilities(&mut capabilities, executor.as_ref());
        if accessibility.is_some() {
            capabilities.accessibility.available = true;
            capabilities.accessibility.version = Some("2".into());
            capabilities.accessibility.limitation = None;
        } else if let Some(failure) = accessibility_failure {
            capabilities.accessibility.available = false;
            capabilities.accessibility.version = None;
            capabilities.accessibility.limitation = Some(failure);
        }
        let (shutdown, _) = watch::channel(false);
        Self {
            controller: Mutex::new(Controller::new(idle_timeout)),
            capabilities,
            inspector,
            capturer,
            accessibility,
            executor,
            execution_quiesced: Notify::new(),
            shutdown,
            started_at: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub async fn finish_execution(&self, connection: waldo_protocol::ConnectionId) {
        self.controller.lock().await.finish_execution(connection);
        self.execution_quiesced.notify_waiters();
    }

    pub async fn abort_execution(
        &self,
        connection: waldo_protocol::ConnectionId,
        release_lease: bool,
    ) {
        self.controller
            .lock()
            .await
            .abort_execution(connection, release_lease);
        self.execution_quiesced.notify_waiters();
    }

    pub async fn wait_for_execution(&self, connection: waldo_protocol::ConnectionId) {
        loop {
            let notified = self.execution_quiesced.notified();
            if !self
                .controller
                .lock()
                .await
                .execution_in_progress(connection)
            {
                return;
            }
            notified.await;
        }
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
        let inspector: Arc<dyn SessionInspector> = Arc::new(NiriInspector);
        let capturer: Arc<dyn ScreenCapturer> = Arc::new(Screencopy);
        let (accessibility, accessibility_failure) = match AccessibilityBackend::initialise() {
            Ok(backend) => (Some(Arc::new(backend)), None),
            Err(error) => (
                None,
                Some(format!(
                    "AT-SPI backend initialisation failed: {}",
                    error.message
                )),
            ),
        };
        let active_keyboard_layout = inspector
            .inspect(false)
            .ok()
            .and_then(|snapshot| snapshot.active_keyboard_layout);
        let executor: Arc<dyn ActionExecutor> = Arc::new(DesktopActionExecutor::initialise(
            accessibility.as_ref().map(Arc::clone),
            active_keyboard_layout,
        ));
        Self::bind_with_runtime_backends(
            path,
            idle_timeout,
            inspector,
            capturer,
            accessibility,
            accessibility_failure,
            executor,
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
        Self::bind_with_all_backends(
            path,
            idle_timeout,
            inspector,
            capturer,
            Arc::new(UnavailableActionExecutor),
        )
    }

    /// Binds every controller backend explicitly for deterministic integration tests.
    pub fn bind_with_all_backends(
        path: impl AsRef<Path>,
        idle_timeout: Duration,
        inspector: Arc<dyn SessionInspector>,
        capturer: Arc<dyn ScreenCapturer>,
        executor: Arc<dyn ActionExecutor>,
    ) -> Result<Self, ServiceError> {
        Self::bind_with_runtime_backends(
            path,
            idle_timeout,
            inspector,
            capturer,
            None,
            None,
            executor,
        )
    }

    fn bind_with_runtime_backends(
        path: impl AsRef<Path>,
        idle_timeout: Duration,
        inspector: Arc<dyn SessionInspector>,
        capturer: Arc<dyn ScreenCapturer>,
        accessibility: Option<Arc<AccessibilityBackend>>,
        accessibility_failure: Option<String>,
        executor: Arc<dyn ActionExecutor>,
    ) -> Result<Self, ServiceError> {
        let path = path.as_ref();
        socket_path::prepare(path)?;

        let listener = UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

        Ok(Self {
            listener,
            state: Arc::new(ServiceState::new(
                idle_timeout,
                inspector,
                capturer,
                accessibility,
                accessibility_failure,
                executor,
            )),
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
        let mut lease_reaper = tokio::time::interval(Duration::from_millis(100));

        let result = loop {
            tokio::select! {
                _ = &mut shutdown => break Ok(()),
                accepted = self.listener.accept() => {
                    let (stream, _) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => break Err(ServiceError::from(error)),
                    };
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
                _ = lease_reaper.tick() => {
                    let expired = self.state
                        .controller
                        .lock()
                        .await
                        .expire_idle_lease(self.state.elapsed());
                    if expired {
                        self.state.executor.release_all();
                    }
                }
            }
        };

        self.state.controller.lock().await.terminate();
        self.state.shutdown.send_replace(true);
        while connections.join_next().await.is_some() {}
        self.state.executor.release_all();
        result
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

pub(crate) fn enable_action_capabilities(
    capabilities: &mut Capabilities,
    executor: &dyn ActionExecutor,
) {
    if executor.supports(waldo_protocol::ActionRoute::VirtualPointer)
        && capabilities.virtual_pointer.version.is_some()
    {
        capabilities.virtual_pointer.available = true;
        capabilities.virtual_pointer.limitation = Some(
            "cross-output drag is refused before dispatch; ordinary cross-output motion is supported"
                .into(),
        );
    }
    if executor.supports(waldo_protocol::ActionRoute::VirtualKeyboard)
        && capabilities.virtual_keyboard.version.is_some()
    {
        capabilities.virtual_keyboard.available = true;
        capabilities.virtual_keyboard.limitation = None;
    }
    if executor.supports(waldo_protocol::ActionRoute::Clipboard)
        && capabilities.clipboard.version.is_some()
    {
        capabilities.clipboard.available = true;
        capabilities.clipboard.limitation = None;
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    state: Arc<ServiceState>,
) -> Result<(), ServiceError> {
    authenticate_peer(&stream)?;

    let mut shutdown = state.shutdown.subscribe();
    if *shutdown.borrow() {
        return Ok(());
    }
    let first: ClientMessage = tokio::select! {
        _ = shutdown.changed() => return Ok(()),
        message = read_message(&mut stream) => message?,
    };
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

    let result = connection_loop(stream, Arc::clone(&state), connection_id).await;
    let _ = state.controller.lock().await.disconnect(connection_id);
    result
}

async fn connection_loop(
    stream: UnixStream,
    state: Arc<ServiceState>,
    connection_id: waldo_protocol::ConnectionId,
) -> Result<(), ServiceError> {
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut shutdown = state.shutdown.subscribe();
    let mut queued = VecDeque::new();
    let mut active_act: Option<tokio::task::JoinHandle<Result<(), TransportError>>> = None;

    loop {
        if *shutdown.borrow() {
            quiesce_connection(&state, connection_id, &mut active_act).await;
            return Ok(());
        }
        if active_act.is_none() {
            let message = if let Some(message) = queued.pop_front() {
                message
            } else {
                tokio::select! {
                    _ = shutdown.changed() => {
                        quiesce_connection(&state, connection_id, &mut active_act).await;
                        return Ok(());
                    }
                    message = read_message(&mut reader) => match message {
                        Ok(message) => message,
                        Err(TransportError::ConnectionClosed) => {
                            quiesce_connection(&state, connection_id, &mut active_act).await;
                            return Ok(());
                        }
                        Err(error) => {
                            quiesce_connection(&state, connection_id, &mut active_act).await;
                            return Err(error.into());
                        }
                    }
                }
            };

            if matches!(&message, ClientMessage::Act { .. }) {
                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
                let request_state = Arc::clone(&state);
                let request_writer = Arc::clone(&writer);
                active_act = Some(tokio::spawn(async move {
                    let response =
                        dispatch(message, &request_state, connection_id, Some(ready_tx)).await;
                    write_message(&mut *request_writer.lock().await, &response).await
                }));
                // Do not accept a later Stop until the earlier Act has either
                // established its cancellation boundary or failed preflight.
                let _ = ready_rx.await;
            } else {
                let response = dispatch(message, &state, connection_id, None).await;
                if let Err(error) = write_message(&mut *writer.lock().await, &response).await {
                    quiesce_connection(&state, connection_id, &mut active_act).await;
                    return Err(error.into());
                }
            }
            continue;
        }

        let active = active_act.as_mut().expect("active Act was checked above");
        tokio::select! {
            _ = shutdown.changed() => {
                quiesce_connection(&state, connection_id, &mut active_act).await;
                return Ok(());
            }
            completed = active => {
                active_act = None;
                match completed {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        quiesce_connection(&state, connection_id, &mut active_act).await;
                        return Err(error.into());
                    }
                    Err(error) => {
                        quiesce_connection(&state, connection_id, &mut active_act).await;
                        return Err(ServiceError::ConnectionTask(error.to_string()));
                    }
                }
            }
            message = read_message(&mut reader) => {
                match message {
                    Ok(message @ ClientMessage::Stop { .. }) => {
                        // Stop is the sole operation allowed to overtake an Act,
                        // and it does not respond until that Act has quiesced.
                        let response = dispatch(message, &state, connection_id, None).await;
                        if let Err(error) = write_message(&mut *writer.lock().await, &response).await {
                            quiesce_connection(&state, connection_id, &mut active_act).await;
                            return Err(error.into());
                        }
                    }
                    Ok(message) => queued.push_back(message),
                    Err(TransportError::ConnectionClosed) => {
                        quiesce_connection(&state, connection_id, &mut active_act).await;
                        return Ok(());
                    }
                    Err(error) => {
                        quiesce_connection(&state, connection_id, &mut active_act).await;
                        return Err(error.into());
                    }
                }
            }
        }
    }
}

async fn quiesce_connection(
    state: &ServiceState,
    connection_id: waldo_protocol::ConnectionId,
    active_act: &mut Option<tokio::task::JoinHandle<Result<(), TransportError>>>,
) {
    let owned_driving_session = {
        let mut controller = state.controller.lock().await;
        let owned = controller
            .driving_session()
            .is_some_and(|session| session.owner() == connection_id);
        let _ = controller.disconnect(connection_id);
        owned
    };
    if let Some(active) = active_act.take() {
        let _ = active.await;
    }
    // An unexpected request-task panic must not leave the exclusion marker set.
    state.finish_execution(connection_id).await;
    if owned_driving_session {
        state.executor.release_all();
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
