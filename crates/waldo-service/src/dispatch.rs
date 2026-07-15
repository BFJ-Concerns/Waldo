use waldo_controller::LeaseError;
use waldo_protocol::{
    ClientMessage, ConnectionId, ErrorCode, HandoverResult, ProtocolError, QueryResult, RequestId,
    ServerMessage, StopResult,
};

use crate::{
    act::act,
    observe::observe,
    server::{ServiceState, enable_capture_capability},
};

pub(crate) async fn dispatch(
    message: ClientMessage,
    state: &ServiceState,
    connection_id: ConnectionId,
    act_ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> ServerMessage {
    let request_id = message.request_id();
    state.cleanup_expired_lease().await;
    match message {
        ClientMessage::Hello { .. } => ServerMessage::Error {
            request_id: Some(request_id),
            error: invalid_request("hello is only valid as the first message"),
        },
        ClientMessage::Observe { request, .. } => {
            observe(
                request_id,
                request.source,
                request.continue_from,
                request.include_accessibility,
                state,
                connection_id,
            )
            .await
        }
        ClientMessage::Act { request, .. } => {
            act(request_id, request, state, connection_id, act_ready).await
        }
        ClientMessage::Query { request, .. } => {
            let inspector = std::sync::Arc::clone(&state.inspector);
            let mut snapshot = match tokio::task::spawn_blocking(move || {
                inspector.inspect(request.include_windows)
            })
            .await
            {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => return inspection_error(request_id, error.to_string()),
                Err(error) => return inspection_error(request_id, error.to_string()),
            };
            enable_capture_capability(&mut snapshot.capabilities, state.capturer.as_ref());
            crate::server::enable_action_capabilities(
                &mut snapshot.capabilities,
                state.executor.as_ref(),
            );
            merge_controller_capabilities(&mut snapshot.capabilities, &state.capabilities);
            let controller = state.controller.lock().await;
            let driving = controller.driving_session();
            ServerMessage::Queried {
                request_id,
                result: Box::new(QueryResult {
                    capabilities: snapshot.capabilities,
                    profile: snapshot.profile,
                    outputs: snapshot.outputs,
                    workspaces: snapshot.workspaces,
                    windows: snapshot.windows,
                    focused_window: snapshot.focused_window,
                    keyboard_layouts: snapshot.keyboard_layouts,
                    active_keyboard_layout: snapshot.active_keyboard_layout,
                    driving_session: driving.map(|session| session.id()),
                    driving_connection: driving.map(|session| session.owner()),
                }),
            }
        }
        ClientMessage::Stop { .. } => stop(request_id, state, connection_id).await,
        ClientMessage::Handover { request, .. } => {
            let previous = {
                let mut controller = state.controller.lock().await;
                if let Err(error) = controller.validate_handover(connection_id, request.target) {
                    return lease_error(request_id, error);
                }
                if let Err(error) = controller.begin_session_cleanup(connection_id) {
                    return lease_error(request_id, error);
                }
                controller.driving_session().map(|session| session.id())
            };
            state.wait_for_execution(connection_id).await;
            state.executor.release_all();
            let mut controller = state.controller.lock().await;
            match controller.handover(connection_id, request.target, state.elapsed()) {
                Ok(session) => ServerMessage::HandedOver {
                    request_id,
                    result: HandoverResult {
                        previous_session: previous.expect("handover requires an active lease"),
                        driving_session: session.id(),
                        owner: session.owner(),
                    },
                },
                Err(error) => lease_error(request_id, error),
            }
        }
    }
}

fn merge_controller_capabilities(
    inspected: &mut waldo_protocol::Capabilities,
    controller: &waldo_protocol::Capabilities,
) {
    inspected.accessibility = controller.accessibility.clone();
}

fn inspection_error(request_id: RequestId, message: String) -> ServerMessage {
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code: ErrorCode::BackendFailure,
            message,
            retryable: true,
        },
    }
}

async fn stop(
    request_id: RequestId,
    state: &ServiceState,
    connection_id: ConnectionId,
) -> ServerMessage {
    let (released, cleanup_started) = {
        let mut controller = state.controller.lock().await;
        let released = controller
            .driving_session()
            .filter(|session| session.owner() == connection_id)
            .map(|session| session.id());
        (released, controller.begin_session_cleanup(connection_id))
    };
    match cleanup_started {
        Ok(()) | Err(LeaseError::NoActiveLease) => {
            state.wait_for_execution(connection_id).await;
            state.executor.release_all();
            let stopped = state.controller.lock().await.stop(connection_id);
            if let Err(error) = stopped
                && error != LeaseError::NoActiveLease
            {
                return lease_error(request_id, error);
            }
            ServerMessage::Stopped {
                request_id,
                result: StopResult {
                    released_session: released,
                },
            }
        }
        Err(error) => lease_error(request_id, error),
    }
}

pub(crate) fn invalid_request(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::InvalidRequest,
        message: message.into(),
        retryable: false,
    }
}

pub(crate) fn lease_error(request_id: RequestId, error: LeaseError) -> ServerMessage {
    let code = match error {
        LeaseError::UnknownConnection(_) => ErrorCode::UnknownConnection,
        LeaseError::HeldBy(_) => ErrorCode::LeaseHeld,
        LeaseError::NoActiveLease | LeaseError::NotOwner { .. } => ErrorCode::NotLeaseOwner,
        LeaseError::ExecutionInProgress | LeaseError::CleanupInProgress => ErrorCode::LeaseHeld,
    };
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code,
            message: error.to_string(),
            retryable: matches!(error, LeaseError::HeldBy(_) | LeaseError::CleanupInProgress),
        },
    }
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{BackendCapability, Capabilities};

    use super::merge_controller_capabilities;

    #[test]
    fn query_capabilities_preserve_the_controller_accessibility_probe() {
        let mut inspected = Capabilities::unavailable();
        inspected.accessibility = BackendCapability::unavailable("AT-SPI has not been probed");
        let mut controller = Capabilities::unavailable();
        controller.accessibility = BackendCapability {
            available: true,
            version: Some("2".into()),
            limitation: None,
        };

        merge_controller_capabilities(&mut inspected, &controller);

        assert_eq!(inspected.accessibility, controller.accessibility);
    }
}
