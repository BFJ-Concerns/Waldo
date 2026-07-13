use std::sync::Arc;

use tokio::sync::oneshot;
use waldo_action::{execute_batch, validate_grounding};
use waldo_protocol::{
    ActRequest, ActResult, ConnectionId, ErrorCode, ProtocolError, RequestId, ServerMessage,
};

use crate::{dispatch::lease_error, observe::capture_observation, server::ServiceState};

pub(crate) async fn act(
    request_id: RequestId,
    request: ActRequest,
    state: &ServiceState,
    connection_id: ConnectionId,
    ready: Option<oneshot::Sender<()>>,
) -> ServerMessage {
    let mut ready = ReadySignal(ready);
    let grounding = {
        let controller = state.controller.lock().await;
        let Some(grounding) = controller
            .observation(connection_id, request.observation_id)
            .cloned()
        else {
            return ServerMessage::Error {
                request_id: Some(request_id),
                error: waldo_action::GroundingFailure::UnknownObservation.protocol_error(),
            };
        };
        grounding
    };
    if !request.verification.is_empty() {
        return error_response(
            request_id,
            ErrorCode::UnsupportedCapability,
            "action verification predicates are not initialised",
            false,
        );
    }

    let actions = request.actions;
    let authority = {
        let mut controller = state.controller.lock().await;
        controller.expire_idle_lease(state.elapsed());
        match controller.begin_execution(connection_id, state.elapsed()) {
            Ok(authority) => authority,
            Err(error) => return lease_error(request_id, error),
        }
    };
    ready.signal();
    let inspector = Arc::clone(&state.inspector);
    let validation_actions = actions.clone();
    let validation_grounding = grounding.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let include_windows = validation_actions
            .iter()
            .any(|action| matches!(action, waldo_protocol::Action::Window { .. }));
        let snapshot = inspector
            .inspect(include_windows)
            .map_err(|error| backend_error(error.to_string()))?;
        validate_grounding(
            Some(&validation_grounding),
            &snapshot.outputs,
            &snapshot.windows,
            &snapshot.workspaces,
            snapshot.focused_window.as_ref(),
            &validation_actions,
        )
        .map_err(|failure| failure.protocol_error())
    })
    .await;
    match checked {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            state
                .abort_execution(connection_id, authority.acquired_lease())
                .await;
            return ServerMessage::Error {
                request_id: Some(request_id),
                error,
            };
        }
        Err(error) => {
            state
                .abort_execution(connection_id, authority.acquired_lease())
                .await;
            return error_response(request_id, ErrorCode::Internal, &error.to_string(), true);
        }
    };
    let session = authority.session();
    let cancellation = authority.cancellation().clone();
    let executor = Arc::clone(&state.executor);
    let execution = tokio::task::spawn_blocking(move || {
        execute_batch(executor.as_ref(), &actions, &cancellation)
    })
    .await;
    let execution = match execution {
        Ok(execution) => execution,
        Err(error) => {
            state.finish_execution(connection_id).await;
            return error_response(request_id, ErrorCode::Internal, &error.to_string(), true);
        }
    };
    let (observation, completion_error, stop_reason) =
        match capture_observation(grounding.source, state, connection_id).await {
            Ok(observation) => (Some(observation), None, execution.stop_reason),
            Err(error) => (
                None,
                Some(error),
                if execution.stop_reason == waldo_protocol::StopReason::Completed {
                    waldo_protocol::StopReason::ExecutionFailed
                } else {
                    execution.stop_reason
                },
            ),
        };
    state.finish_execution(connection_id).await;

    ServerMessage::Acted {
        request_id,
        result: ActResult {
            driving_session: session.id(),
            receipts: execution.receipts,
            first_not_executed: execution.first_not_executed,
            stop_reason,
            observation,
            completion_error,
            verification: Vec::new(),
        },
    }
}

struct ReadySignal(Option<oneshot::Sender<()>>);

impl ReadySignal {
    fn signal(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReadySignal {
    fn drop(&mut self) {
        self.signal();
    }
}

fn backend_error(message: String) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message,
        retryable: true,
    }
}

fn error_response(
    request_id: RequestId,
    code: ErrorCode,
    message: &str,
    retryable: bool,
) -> ServerMessage {
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code,
            message: message.into(),
            retryable,
        },
    }
}
