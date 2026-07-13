use waldo_controller::LeaseError;
use waldo_protocol::{
    ClientMessage, ConnectionId, ErrorCode, HandoverResult, ProtocolError, QueryResult, RequestId,
    ServerMessage, StopResult,
};

use crate::server::ServiceState;

pub(crate) async fn dispatch(
    message: ClientMessage,
    state: &ServiceState,
    connection_id: ConnectionId,
) -> ServerMessage {
    let request_id = message.request_id();
    match message {
        ClientMessage::Hello { .. } => ServerMessage::Error {
            request_id: Some(request_id),
            error: invalid_request("hello is only valid as the first message"),
        },
        ClientMessage::Observe { .. } => unsupported(request_id, "screencopy backend"),
        ClientMessage::Act { .. } => unsupported(request_id, "desktop action backends"),
        ClientMessage::Query { request, .. } => {
            let inspector = std::sync::Arc::clone(&state.inspector);
            let snapshot = match tokio::task::spawn_blocking(move || {
                inspector.inspect(request.include_windows)
            })
            .await
            {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => return inspection_error(request_id, error.to_string()),
                Err(error) => return inspection_error(request_id, error.to_string()),
            };
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
            let mut controller = state.controller.lock().await;
            let previous = controller.driving_session().map(|session| session.id());
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
    let mut controller = state.controller.lock().await;
    let released = controller
        .driving_session()
        .filter(|session| session.owner() == connection_id)
        .map(|session| session.id());
    match controller.stop(connection_id) {
        Ok(()) | Err(LeaseError::NoActiveLease) => ServerMessage::Stopped {
            request_id,
            result: StopResult {
                released_session: released,
            },
        },
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

fn unsupported(request_id: RequestId, capability: &str) -> ServerMessage {
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: format!("{capability} is not initialised"),
            retryable: false,
        },
    }
}

fn lease_error(request_id: RequestId, error: LeaseError) -> ServerMessage {
    let code = match error {
        LeaseError::UnknownConnection(_) => ErrorCode::UnknownConnection,
        LeaseError::HeldBy(_) => ErrorCode::LeaseHeld,
        LeaseError::NoActiveLease | LeaseError::NotOwner { .. } => ErrorCode::NotLeaseOwner,
    };
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code,
            message: error.to_string(),
            retryable: matches!(error, LeaseError::HeldBy(_)),
        },
    }
}
