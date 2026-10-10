use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::oneshot;
use waldo_action::{execute_batch, validate_grounding};
use waldo_protocol::{
    ActRequest, ActResult, ApplicationFrameState, ApplicationOutcome, CompletionEvidence,
    ConnectionId, ErrorCode, EvidenceAgreement, ProtocolError, RequestId, ServerMessage,
};

use crate::{
    batch::BatchActionExecutor,
    dispatch::lease_error,
    observe::{CapturedObservation, capture_observation},
    server::ServiceState,
    verification::{VerificationPlan, failed_verification},
};

const POST_ACTION_FRAME_TIMEOUT: Duration = Duration::from_secs(1);
const POST_ACTION_FRAME_POLL_INTERVAL: Duration = Duration::from_millis(20);
const POST_ACTION_FRAME_QUIET_PERIOD: Duration = Duration::from_millis(150);

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
    if let Err(error) =
        validate_action_order(&actions).and_then(|()| validate_wait_states(&actions))
    {
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
    let result_source = actions
        .iter()
        .rev()
        .find_map(|action| match action {
            waldo_protocol::Action::Capture { source } => Some(source.clone()),
            _ => None,
        })
        .unwrap_or_else(|| grounding.source.clone());
    let verification = match VerificationPlan::prepare(
        verification,
        &result_source,
        grounding.include_accessibility,
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
    state.cleanup_expired_lease().await;
    let authority = {
        let mut controller = state.controller.lock().await;
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
            .map_err(|error| ProtocolError::backend_failure(error.to_string(), true))?;
        validate_grounding(
            &validation_grounding,
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
    let expect_visual_change = actions_expect_visual_change(&actions);
    let executor = Arc::clone(&state.executor);
    let batch_executor = BatchActionExecutor::new(
        executor,
        Arc::clone(&state.inspector),
        Arc::clone(&state.capturer),
        state.accessibility.as_ref().map(Arc::clone),
        grounding.clone(),
        referenced_frames,
    );
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
    let (observation, frame_state, settle_deadline_reached, mut completion_error, mut stop_reason) =
        match capture_post_action_observation(
            result_source,
            grounding.include_accessibility,
            grounding.frame_fingerprint,
            grounding.application_frame_fingerprint,
            expect_visual_change,
            state,
            connection_id,
        )
        .await
        {
            Ok(post_action) => (
                Some(post_action.captured),
                post_action.frame_state,
                post_action.settle_deadline_reached,
                None,
                execution.stop_reason,
            ),
            Err(error) => (
                None,
                ApplicationFrameState::Unchanged,
                false,
                Some(error),
                if execution.stop_reason == waldo_protocol::StopReason::Completed {
                    waldo_protocol::StopReason::ExecutionFailed
                } else {
                    execution.stop_reason
                },
            ),
        };
    let verification_results = if let Some(captured) = &observation {
        match verification.evaluate(captured, state).await {
            Ok(results) => results,
            Err(error) => {
                // A batch that already stopped keeps its own stop reason, but
                // the verification failure is still reported beside it.
                if stop_reason == waldo_protocol::StopReason::Completed {
                    stop_reason = waldo_protocol::StopReason::VerificationFailed;
                }
                completion_error = Some(error);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let observation = observation.map(|captured| captured.observation);
    let verification_agreement = evidence_agreement(&verification_results);
    let application_outcome = application_outcome(
        stop_reason,
        frame_state,
        settle_deadline_reached,
        &verification_results,
    );
    if application_outcome == ApplicationOutcome::Failed
        && stop_reason == waldo_protocol::StopReason::Completed
    {
        stop_reason = waldo_protocol::StopReason::VerificationFailed;
        completion_error = Some(failed_verification());
    }
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
            application_outcome,
            completion_evidence: CompletionEvidence {
                frame_state,
                settle_deadline_reached,
                verification_agreement,
            },
        },
    }
}

async fn capture_post_action_observation(
    source: waldo_protocol::SourceSelector,
    include_accessibility: bool,
    previous_frame: u64,
    previous_application_frame: u64,
    expect_visual_change: bool,
    state: &ServiceState,
    connection_id: ConnectionId,
) -> Result<PostActionObservation, ProtocolError> {
    let deadline = Instant::now() + POST_ACTION_FRAME_TIMEOUT;
    let mut latest_frame = previous_frame;
    let mut latest_application_frame = previous_application_frame;
    let mut last_change = Instant::now();
    let mut changed = false;
    loop {
        let captured =
            capture_observation(source.clone(), include_accessibility, state, connection_id)
                .await?;
        if !expect_visual_change {
            return Ok(PostActionObservation {
                captured,
                frame_state: ApplicationFrameState::Unchanged,
                settle_deadline_reached: false,
            });
        }
        let now = Instant::now();
        let frame = captured.frame_fingerprint;
        let application_frame = captured.application_frame_fingerprint;
        let composite_changed = frame != latest_frame;
        if composite_changed {
            latest_frame = frame;
        }
        if application_frame != latest_application_frame {
            latest_application_frame = application_frame;
            last_change = now;
            changed = true;
        }
        if changed && now.duration_since(last_change) >= POST_ACTION_FRAME_QUIET_PERIOD {
            return Ok(PostActionObservation {
                captured,
                frame_state: ApplicationFrameState::Settled,
                settle_deadline_reached: false,
            });
        }
        if now >= deadline {
            return Ok(PostActionObservation {
                captured,
                frame_state: if changed {
                    ApplicationFrameState::Changing
                } else if composite_changed || frame != previous_frame {
                    ApplicationFrameState::CursorOnly
                } else {
                    ApplicationFrameState::Unchanged
                },
                settle_deadline_reached: true,
            });
        }
        tokio::time::sleep(POST_ACTION_FRAME_POLL_INTERVAL).await;
    }
}

struct PostActionObservation {
    captured: CapturedObservation,
    frame_state: ApplicationFrameState,
    settle_deadline_reached: bool,
}

fn evidence_agreement(results: &[bool]) -> EvidenceAgreement {
    match results.first() {
        None => EvidenceAgreement::NotEvaluated,
        Some(first) if results.iter().all(|result| result == first) => EvidenceAgreement::Agreed,
        Some(_) => EvidenceAgreement::Contradictory,
    }
}

fn application_outcome(
    stop_reason: waldo_protocol::StopReason,
    frame_state: ApplicationFrameState,
    settle_deadline_reached: bool,
    verification: &[bool],
) -> ApplicationOutcome {
    if stop_reason != waldo_protocol::StopReason::Completed {
        return ApplicationOutcome::Inconclusive;
    }
    if settle_deadline_reached && frame_state == ApplicationFrameState::Changing {
        return ApplicationOutcome::Inconclusive;
    }
    match verification.first() {
        None => ApplicationOutcome::Inconclusive,
        Some(true) if verification.iter().all(|result| *result) => ApplicationOutcome::Completed,
        Some(false) if verification.iter().all(|result| !*result) => ApplicationOutcome::Failed,
        Some(_) => ApplicationOutcome::Inconclusive,
    }
}

/// Whether an action drives the pointer, and so depends on visual
/// coordinates and moves the cursor.
fn is_pointer_action(action: &waldo_protocol::Action) -> bool {
    matches!(
        action,
        waldo_protocol::Action::PointerMoveAbsolute { .. }
            | waldo_protocol::Action::PointerMoveRelative { .. }
            | waldo_protocol::Action::PointerButton { .. }
            | waldo_protocol::Action::PointerClick { .. }
            | waldo_protocol::Action::PointerDrag { .. }
            | waldo_protocol::Action::PointerPath { .. }
            | waldo_protocol::Action::Scroll { .. }
    )
}

fn actions_expect_visual_change(actions: &[waldo_protocol::Action]) -> bool {
    actions.iter().any(|action| {
        is_pointer_action(action)
            || matches!(
                action,
                waldo_protocol::Action::Key { .. }
                    | waldo_protocol::Action::KeyChord { .. }
                    | waldo_protocol::Action::Text { .. }
                    | waldo_protocol::Action::Accessibility { .. }
                    | waldo_protocol::Action::Window { .. }
                    | waldo_protocol::Action::Workspace { .. }
            )
    })
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

/// Refuses a wait on an AT-SPI state name that does not exist before any
/// action runs, rather than when the batch reaches the wait.
fn validate_wait_states(actions: &[waldo_protocol::Action]) -> Result<(), ProtocolError> {
    actions
        .iter()
        .filter_map(|action| match action {
            waldo_protocol::Action::Wait {
                predicate: waldo_protocol::Predicate::ElementState { state, .. },
                ..
            } => Some(state),
            _ => None,
        })
        .try_for_each(|state| waldo_accessibility::require_known_state(state))
}

fn validate_action_order(actions: &[waldo_protocol::Action]) -> Result<(), ProtocolError> {
    let mut waited = false;
    for action in actions {
        if matches!(action, waldo_protocol::Action::Wait { .. }) {
            waited = true;
        } else if waited && is_pointer_action(action) {
            return Err(ProtocolError {
                retryable: true,
                ..ProtocolError::invalid_request(
                    "a pointer action cannot follow a wait in the same batch; observe again before replanning visual coordinates",
                )
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{
        Action, ApplicationFrameState, ApplicationOutcome, LogicalPoint, Predicate, StopReason,
        WindowId,
    };

    use super::{actions_expect_visual_change, application_outcome, validate_action_order};

    #[test]
    fn a_changing_application_at_the_settle_deadline_is_inconclusive() {
        assert_eq!(
            application_outcome(
                StopReason::Completed,
                ApplicationFrameState::Changing,
                true,
                &[true, true]
            ),
            ApplicationOutcome::Inconclusive
        );
        assert_eq!(
            application_outcome(
                StopReason::Completed,
                ApplicationFrameState::Changing,
                true,
                &[false, false]
            ),
            ApplicationOutcome::Inconclusive
        );
        assert_eq!(
            application_outcome(
                StopReason::Completed,
                ApplicationFrameState::Changing,
                true,
                &[]
            ),
            ApplicationOutcome::Inconclusive
        );
    }

    #[test]
    fn pointer_motion_waits_for_its_cursor_frame() {
        assert!(actions_expect_visual_change(&[
            Action::PointerMoveAbsolute {
                position: LogicalPoint { x: 10.0, y: 20.0 },
            },
        ]));
    }

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
