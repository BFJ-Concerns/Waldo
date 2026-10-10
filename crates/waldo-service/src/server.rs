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
    Capabilities, ClientMessage, ErrorCode, ProtocolError, RequestId, SCHEMA_MAJOR_VERSION,
    ServerMessage,
};
use waldo_screencopy::{ScreenCapturer, Screencopy, UnavailableCapturer};
use waldo_socket::{TransportError, read_frame, write_message};
use waldo_virtual_keyboard::{ActiveLayout, ActiveLayoutSource};

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
            .unwrap_or_else(|error| {
                eprintln!("Waldo controller could not inspect the desktop at start-up: {error}");
                Capabilities::unavailable_because(format!(
                    "desktop inspection failed at start-up: {error}"
                ))
            });
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

    /// Restores backend state before an expired driving lease releases authority.
    pub async fn cleanup_expired_lease(&self) {
        let owner = self
            .controller
            .lock()
            .await
            .begin_idle_cleanup(self.elapsed());
        let Some(owner) = owner else {
            return;
        };
        report_unfinished_cleanup("expired driving lease", self.executor.release_all());
        let _ = self.controller.lock().await.stop(owner);
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
    pub fn bind(
        path: impl AsRef<Path>,
        idle_timeout: Duration,
        settings: &waldo_settings::Settings,
    ) -> Result<Self, ServiceError> {
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
        let active_layout: ActiveLayoutSource = Arc::new(|| {
            waldo_niri::active_keyboard_layout()
                .map(|layout| ActiveLayout {
                    index: layout.index,
                    name: layout.name,
                })
                .map_err(|error| ProtocolError {
                    code: ErrorCode::BackendFailure,
                    message: format!("read the active keyboard layout from Niri: {error}"),
                    retryable: true,
                })
        });
        let executor: Arc<dyn ActionExecutor> = Arc::new(DesktopActionExecutor::initialise(
            accessibility.as_ref().map(Arc::clone),
            active_layout,
            settings,
        )?);
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
                    self.state.cleanup_expired_lease().await;
                }
            }
        };

        {
            let mut controller = self.state.controller.lock().await;
            if let Some(owner) = controller.driving_session().map(|session| session.owner()) {
                let _ = controller.begin_session_cleanup(owner);
            }
        }
        self.state.shutdown.send_replace(true);
        while connections.join_next().await.is_some() {}
        report_unfinished_cleanup("controller shutdown", self.state.executor.release_all());
        self.state.controller.lock().await.terminate();
        result
    }
}

/// Cleanup with no client left to answer goes to the service log, so a
/// clipboard the operator must restore by hand is never lost silently.
fn report_unfinished_cleanup(boundary: &str, limitation: Option<String>) {
    if let Some(limitation) = limitation {
        eprintln!("Waldo controller cleanup after {boundary} left state unrestored: {limitation}");
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
    if capabilities.virtual_pointer.version.is_some() {
        if let Some(limitation) = executor.limitation(waldo_protocol::ActionRoute::VirtualPointer) {
            capabilities.virtual_pointer.available = false;
            capabilities.virtual_pointer.limitation = Some(limitation);
        } else if executor.supports(waldo_protocol::ActionRoute::VirtualPointer) {
            capabilities.virtual_pointer.available = true;
            capabilities.virtual_pointer.limitation = Some(
                "cross-output drag is refused before dispatch; ordinary cross-output motion is supported"
                    .into(),
            );
        }
    }
    if capabilities.virtual_keyboard.version.is_some() {
        if let Some(limitation) = executor.limitation(waldo_protocol::ActionRoute::VirtualKeyboard)
        {
            capabilities.virtual_keyboard.available = false;
            capabilities.virtual_keyboard.limitation = Some(limitation);
        } else if executor.supports(waldo_protocol::ActionRoute::VirtualKeyboard) {
            capabilities.virtual_keyboard.available = true;
            capabilities.virtual_keyboard.limitation = None;
        }
    }
    if capabilities.clipboard.version.is_some() {
        if let Some(limitation) = executor.limitation(waldo_protocol::ActionRoute::Clipboard) {
            capabilities.clipboard.available = false;
            capabilities.clipboard.limitation = Some(limitation);
        } else if executor.supports(waldo_protocol::ActionRoute::Clipboard) {
            capabilities.clipboard.available = true;
            capabilities.clipboard.limitation =
                executor.caveat(waldo_protocol::ActionRoute::Clipboard);
        }
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
    // The handshake's client identity is validated on decoding; the
    // controller tracks connections, not their logical names.
    let request_id = loop {
        let first = tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            message = read_client_message(&mut stream) => message?,
        };
        match first {
            IncomingMessage::Valid(ClientMessage::Hello {
                request_id,
                schema_major,
                ..
            }) if schema_major == SCHEMA_MAJOR_VERSION => break request_id,
            IncomingMessage::Valid(ClientMessage::Hello {
                request_id,
                schema_major,
                ..
            }) => {
                write_message(
                    &mut stream,
                    &ServerMessage::Error {
                        request_id: Some(request_id),
                        error: ProtocolError::incompatible_schema(
                            SCHEMA_MAJOR_VERSION,
                            schema_major,
                        ),
                    },
                )
                .await?;
                return Ok(());
            }
            IncomingMessage::Valid(other) => {
                write_message(
                    &mut stream,
                    &invalid_request_response(
                        Some(other.request_id()),
                        "the first message must be hello",
                    ),
                )
                .await?;
            }
            IncomingMessage::Invalid(response) => {
                write_message(&mut stream, &response).await?;
            }
        }
    };

    let connection_id = state.controller.lock().await.connect();
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
                    message = read_client_message(&mut reader) => match message {
                        Ok(IncomingMessage::Valid(message)) => message,
                        Ok(IncomingMessage::Invalid(response)) => {
                            if let Err(error) = write_message(&mut *writer.lock().await, &response).await {
                                quiesce_connection(&state, connection_id, &mut active_act).await;
                                return Err(error.into());
                            }
                            continue;
                        }
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
            message = read_client_message(&mut reader) => {
                match message {
                    Ok(IncomingMessage::Valid(message @ (ClientMessage::Stop { .. } | ClientMessage::Handover { .. }))) => {
                        // Authority-ending operations may overtake an Act to
                        // cancel it, but cannot respond until it has quiesced.
                        let response = dispatch(message, &state, connection_id, None).await;
                        if let Err(error) = write_message(&mut *writer.lock().await, &response).await {
                            quiesce_connection(&state, connection_id, &mut active_act).await;
                            return Err(error.into());
                        }
                    }
                    Ok(IncomingMessage::Valid(message)) => queued.push_back(message),
                    Ok(IncomingMessage::Invalid(response)) => {
                        if let Err(error) = write_message(&mut *writer.lock().await, &response).await {
                            quiesce_connection(&state, connection_id, &mut active_act).await;
                            return Err(error.into());
                        }
                    }
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

enum IncomingMessage {
    Valid(ClientMessage),
    Invalid(Box<ServerMessage>),
}

async fn read_client_message<R>(reader: &mut R) -> Result<IncomingMessage, TransportError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let payload = read_frame(reader).await?;
    let value: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(value) => value,
        Err(_) => {
            return Ok(IncomingMessage::Invalid(Box::new(
                invalid_request_response(None, "request frame is not valid JSON"),
            )));
        }
    };
    let request_id = value
        .get("request_id")
        .and_then(serde_json::Value::as_u64)
        .map(RequestId::new);
    match serde_json::from_value(value) {
        Ok(message) => Ok(IncomingMessage::Valid(message)),
        Err(_) => Ok(IncomingMessage::Invalid(Box::new(
            invalid_request_response(request_id, "request does not match the protocol schema"),
        ))),
    }
}

fn invalid_request_response(request_id: Option<RequestId>, message: &str) -> ServerMessage {
    ServerMessage::Error {
        request_id,
        error: crate::dispatch::invalid_request(message),
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
        if owned {
            let _ = controller.begin_session_cleanup(connection_id);
        } else {
            let _ = controller.disconnect(connection_id);
        }
        owned
    };
    if let Some(active) = active_act.take() {
        let _ = active.await;
    }
    // An unexpected request-task panic must not leave the exclusion marker set.
    state.finish_execution(connection_id).await;
    if owned_driving_session {
        report_unfinished_cleanup("disconnected driver", state.executor.release_all());
        let _ = state.controller.lock().await.disconnect(connection_id);
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

#[cfg(test)]
mod tests {
    use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
    use waldo_controller::ExecutionToken;
    use waldo_protocol::{Action, ActionRoute, Capabilities};

    use super::enable_action_capabilities;

    struct BlockedClipboard;

    impl ActionExecutor for BlockedClipboard {
        fn execute(
            &self,
            _action: &Action,
            _cancellation: &ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            unreachable!("capability test does not execute actions")
        }

        fn supports(&self, route: ActionRoute) -> bool {
            route == ActionRoute::Clipboard
        }

        fn limitation(&self, route: ActionRoute) -> Option<String> {
            (route == ActionRoute::Clipboard)
                .then(|| "clipboard mutation blocked by unresolved restoration".into())
        }
    }

    #[test]
    fn query_clipboard_capability_surfaces_payload_free_restoration_failure() {
        let mut capabilities = Capabilities::unavailable();
        capabilities.clipboard.version = Some("runtime-discovered".into());

        enable_action_capabilities(&mut capabilities, &BlockedClipboard);

        assert!(!capabilities.clipboard.available);
        assert_eq!(
            capabilities.clipboard.limitation.as_deref(),
            Some("clipboard mutation blocked by unresolved restoration")
        );
    }

    struct FailedBackends;

    impl ActionExecutor for FailedBackends {
        fn execute(
            &self,
            _action: &Action,
            _cancellation: &ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            unreachable!("capability test does not execute actions")
        }

        fn supports(&self, _route: ActionRoute) -> bool {
            false
        }

        fn limitation(&self, route: ActionRoute) -> Option<String> {
            matches!(route, ActionRoute::VirtualPointer | ActionRoute::Clipboard)
                .then(|| format!("{route:?} initialisation failed: compositor went away"))
        }
    }

    #[test]
    fn query_names_why_an_advertised_backend_failed_to_initialise() {
        let mut capabilities = Capabilities::unavailable();
        capabilities.virtual_pointer.version = Some("runtime-discovered".into());
        capabilities.clipboard.version = Some("runtime-discovered".into());

        enable_action_capabilities(&mut capabilities, &FailedBackends);

        assert!(!capabilities.virtual_pointer.available);
        assert_eq!(
            capabilities.virtual_pointer.limitation.as_deref(),
            Some("VirtualPointer initialisation failed: compositor went away")
        );
        assert!(!capabilities.clipboard.available);
        assert_eq!(
            capabilities.clipboard.limitation.as_deref(),
            Some("Clipboard initialisation failed: compositor went away")
        );
    }
}
