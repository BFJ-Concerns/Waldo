use std::{collections::BTreeMap, sync::Arc};

use tokio::sync::oneshot;
use waldo_action::{execute_batch, validate_grounding};
use waldo_protocol::{
    ActRequest, ActResult, ConnectionId, ErrorCode, ProtocolError, RequestId, ServerMessage,
};

use crate::{
    batch::BatchActionExecutor,
    dispatch::lease_error,
    observe::capture_observation,
    server::ServiceState,
    verification::{VerificationPlan, failed_verification},
};

pub(crate) async fn act(
    request_id: RequestId,
    request: ActRequest,
    state: &ServiceState,
    connection_id: ConnectionId,
    ready: Option<oneshot::Sender<()>>,
) -> ServerMessage {
    let mut ready = ReadySignal(ready);
    let ActRequest {
        observation_id,
        actions,
        verification,
    } = request;
    if let Err(error) = validate_action_order(&actions) {
        return ServerMessage::Error {
            request_id: Some(request_id),
            error,
        };
    }
    let (grounding, referenced_frames) = {
        let controller = state.controller.lock().await;
        let Some(grounding) = controller
            .observation(connection_id, observation_id)
            .cloned()
        else {
            return ServerMessage::Error {
                request_id: Some(request_id),
                error: waldo_action::GroundingFailure::UnknownObservation.protocol_error(),
            };
        };
        let mut referenced_frames = BTreeMap::new();
        for from in actions.iter().filter_map(|action| match action {
            waldo_protocol::Action::Wait {
                predicate: waldo_protocol::Predicate::FrameChanged { from },
                ..
            } => Some(*from),
            _ => None,
        }) {
            let Some(frame) = controller.observation(connection_id, from).cloned() else {
                return ServerMessage::Error {
                    request_id: Some(request_id),
                    error: waldo_action::GroundingFailure::UnknownObservation.protocol_error(),
                };
            };
            referenced_frames.insert(from, frame);
        }
        (grounding, referenced_frames)
    };
    let verification = match VerificationPlan::prepare(
        verification,
        &grounding.source,
        state,
        connection_id,
    )
    .await
    {
        Ok(verification) => verification,
        Err(error) => {
            return ServerMessage::Error {
                request_id: Some(request_id),
                error,
            };
        }
    };
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
        let include_windows = validation_actions.iter().any(|action| {
            matches!(
                action,
                waldo_protocol::Action::Window { .. } | waldo_protocol::Action::Workspace { .. }
            )
        }) || matches!(
            &validation_grounding.source,
            waldo_protocol::SourceSelector::Window { .. }
        );
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
    let batch_executor = BatchActionExecutor::new(
        executor,
        Arc::clone(&state.inspector),
        Arc::clone(&state.capturer),
        state.accessibility.as_ref().map(Arc::clone),
        grounding.clone(),
        referenced_frames,
    );
    let result_source = actions
        .iter()
        .rev()
        .find_map(|action| match action {
            waldo_protocol::Action::Capture { source } => Some(source.clone()),
            _ => None,
        })
        .unwrap_or_else(|| grounding.source.clone());
    let execution = tokio::task::spawn_blocking(move || {
        execute_batch(&batch_executor, &actions, &cancellation)
    })
    .await;
    let execution = match execution {
        Ok(execution) => execution,
        Err(error) => {
            state.finish_execution(connection_id).await;
            return error_response(request_id, ErrorCode::Internal, &error.to_string(), true);
        }
    };
    let (observation, mut completion_error, mut stop_reason) = match capture_observation(
        result_source,
        grounding.include_accessibility,
        state,
        connection_id,
    )
    .await
    {
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
    let verification_results = if let Some(observation) = &observation {
        match verification
            .evaluate(observation, state, connection_id)
            .await
        {
            Ok(results) => {
                if results.iter().any(|passed| !passed)
                    && stop_reason == waldo_protocol::StopReason::Completed
                {
                    stop_reason = waldo_protocol::StopReason::VerificationFailed;
                    completion_error = Some(failed_verification());
                }
                results
            }
            Err(error) => {
                if stop_reason == waldo_protocol::StopReason::Completed {
                    stop_reason = waldo_protocol::StopReason::VerificationFailed;
                    completion_error = Some(error);
                }
                Vec::new()
            }
        }
    } else {
        Vec::new()
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
            verification: verification_results,
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

fn validate_action_order(actions: &[waldo_protocol::Action]) -> Result<(), ProtocolError> {
    let mut waited = false;
    for action in actions {
        if matches!(action, waldo_protocol::Action::Wait { .. }) {
            waited = true;
        } else if waited
            && matches!(
                action,
                waldo_protocol::Action::PointerMoveAbsolute { .. }
                    | waldo_protocol::Action::PointerMoveRelative { .. }
                    | waldo_protocol::Action::PointerButton { .. }
                    | waldo_protocol::Action::PointerClick { .. }
                    | waldo_protocol::Action::PointerDrag { .. }
                    | waldo_protocol::Action::PointerPath { .. }
                    | waldo_protocol::Action::Scroll { .. }
            )
        {
            return Err(ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: "a pointer action cannot follow a wait in the same batch; observe again before replanning visual coordinates".into(),
                retryable: true,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{Action, LogicalPoint, Predicate, WindowId};

    use super::validate_action_order;

    #[test]
    fn visual_coordinates_cannot_cross_a_wait_boundary() {
        let actions = [
            Action::Wait {
                predicate: Predicate::FocusedWindow {
                    window_id: WindowId("7".into()),
                },
                timeout_ms: 100,
            },
            Action::PointerMoveAbsolute {
                position: LogicalPoint { x: 10.0, y: 20.0 },
            },
        ];

        assert!(validate_action_order(&actions).is_err());
        assert!(validate_action_order(&actions[..1]).is_ok());
    }
}
