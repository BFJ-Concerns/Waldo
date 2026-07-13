//! Backend-independent safety checks for observation-grounded action batches.

use waldo_controller::{ExecutionToken, ObservationGrounding};
use waldo_protocol::{
    Action, ActionReceipt, ActionRoute, ErrorCode, LogicalPoint, LogicalRect, OutputState,
    ProtocolError, ReceiptStatus, StopReason, WindowId,
};

/// Backend seam beneath the controller-owned ordering and safety policy.
pub trait ActionExecutor: Send + Sync {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionRoute, ProtocolError>;
}

/// Explicit fail-closed executor used until a live action route passes its gate.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableActionExecutor;

impl ActionExecutor for UnavailableActionExecutor {
    fn execute(
        &self,
        _action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionRoute, ProtocolError> {
        Err(ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: "no desktop action backend is initialised".into(),
            retryable: false,
        })
    }
}

/// Mechanical result of attempting one ordered batch up to its first failure.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchExecution {
    pub receipts: Vec<ActionReceipt>,
    pub first_not_executed: Option<usize>,
    pub stop_reason: StopReason,
}

pub fn execute_batch(
    executor: &dyn ActionExecutor,
    actions: &[Action],
    cancellation: &ExecutionToken,
) -> BatchExecution {
    let mut receipts = Vec::with_capacity(actions.len());
    let mut first_not_executed = None;
    for (index, action) in actions.iter().enumerate() {
        if cancellation.is_cancelled() {
            first_not_executed = Some(index);
            receipts.extend((index..actions.len()).map(|index| ActionReceipt {
                index,
                status: ReceiptStatus::NotExecuted,
                route: None,
                error: None,
            }));
            break;
        }
        match executor.execute(action, cancellation) {
            Ok(route) => receipts.push(ActionReceipt {
                index,
                status: ReceiptStatus::Executed,
                route: Some(route),
                error: None,
            }),
            Err(error) => {
                first_not_executed = Some(index);
                receipts.push(ActionReceipt {
                    index,
                    status: ReceiptStatus::Failed,
                    route: None,
                    error: Some(error),
                });
                receipts.extend((index + 1..actions.len()).map(|index| ActionReceipt {
                    index,
                    status: ReceiptStatus::NotExecuted,
                    route: None,
                    error: None,
                }));
                break;
            }
        }
    }
    BatchExecution {
        receipts,
        first_not_executed,
        stop_reason: if cancellation.is_cancelled() {
            StopReason::Cancelled
        } else if first_not_executed.is_some() {
            StopReason::ExecutionFailed
        } else {
            StopReason::Completed
        },
    }
}

/// A mechanical reason an observation can no longer authorise its batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroundingFailure {
    UnknownObservation,
    TargetLost,
    GeometryChanged,
    FocusChanged,
    TargetOutsideObservation,
}

impl GroundingFailure {
    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::UnknownObservation => ErrorCode::StaleObservation,
            Self::TargetLost => ErrorCode::TargetLost,
            Self::GeometryChanged => ErrorCode::GeometryChanged,
            Self::FocusChanged => ErrorCode::FocusChanged,
            Self::TargetOutsideObservation => ErrorCode::InvalidRequest,
        }
    }

    pub const fn stop_reason(self) -> StopReason {
        match self {
            Self::UnknownObservation => StopReason::StaleObservation,
            Self::TargetLost => StopReason::TargetLost,
            Self::GeometryChanged => StopReason::GeometryChanged,
            Self::FocusChanged => StopReason::FocusChanged,
            Self::TargetOutsideObservation => StopReason::ExecutionFailed,
        }
    }

    pub fn protocol_error(self) -> ProtocolError {
        let message = match self {
            Self::UnknownObservation => "the cited observation is no longer retained",
            Self::TargetLost => "the observed output is no longer available",
            Self::GeometryChanged => {
                "the observed output geometry or coordinate transform has changed"
            }
            Self::FocusChanged => "foreground focus changed after the cited observation",
            Self::TargetOutsideObservation => {
                "an absolute pointer target lies outside the pixels in the cited observation"
            }
        };
        ProtocolError {
            code: self.error_code(),
            message: message.into(),
            retryable: true,
        }
    }
}

/// Revalidates facts that can redirect an otherwise valid action.
pub fn validate_grounding(
    recorded: Option<&ObservationGrounding>,
    outputs: &[OutputState],
    focused_window: Option<&WindowId>,
    actions: &[Action],
) -> Result<(), GroundingFailure> {
    let recorded = recorded.ok_or(GroundingFailure::UnknownObservation)?;
    let output = outputs
        .iter()
        .find(|output| output.id == recorded.output_id)
        .ok_or(GroundingFailure::TargetLost)?;
    if output.logical_bounds != recorded.output_logical_bounds
        || output.scale != recorded.scale
        || output.transform != recorded.transform
    {
        return Err(GroundingFailure::GeometryChanged);
    }
    if actions_require_stable_focus(actions) && focused_window != recorded.focused_window.as_ref() {
        return Err(GroundingFailure::FocusChanged);
    }
    if !pointer_targets_within(
        actions,
        recorded.observed_logical_bounds,
        recorded.cursor_position,
    ) {
        return Err(GroundingFailure::TargetOutsideObservation);
    }
    Ok(())
}

fn pointer_targets_within(
    actions: &[Action],
    bounds: LogicalRect,
    mut cursor: Option<LogicalPoint>,
) -> bool {
    for action in actions {
        match action {
            Action::PointerMoveAbsolute { position } | Action::PointerClick { position, .. } => {
                if !point_within(*position, bounds) {
                    return false;
                }
                cursor = Some(*position);
            }
            Action::PointerDrag { from, to, .. } => {
                if !point_within(*from, bounds) || !point_within(*to, bounds) {
                    return false;
                }
                cursor = Some(*to);
            }
            Action::PointerPath { points, .. } => {
                if !points
                    .iter()
                    .copied()
                    .all(|point| point_within(point, bounds))
                {
                    return false;
                }
                cursor = points.last().copied().or(cursor);
            }
            Action::PointerMoveRelative { delta } => {
                let Some(current) = cursor else {
                    return false;
                };
                let destination = LogicalPoint {
                    x: current.x + delta.x,
                    y: current.y + delta.y,
                };
                if !point_within(destination, bounds) {
                    return false;
                }
                cursor = Some(destination);
            }
            Action::PointerButton { .. } | Action::Scroll { .. }
                if !cursor.is_some_and(|point| point_within(point, bounds)) =>
            {
                return false;
            }
            _ => {}
        }
    }
    true
}

fn point_within(point: LogicalPoint, bounds: LogicalRect) -> bool {
    point.x.is_finite()
        && point.y.is_finite()
        && point.x >= bounds.origin.x
        && point.y >= bounds.origin.y
        && point.x < bounds.origin.x + bounds.width
        && point.y < bounds.origin.y + bounds.height
}

/// Input directed at application content must not drift to a newly focused window.
fn actions_require_stable_focus(actions: &[Action]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            Action::PointerMoveAbsolute { .. }
                | Action::PointerMoveRelative { .. }
                | Action::PointerButton { .. }
                | Action::PointerClick { .. }
                | Action::PointerDrag { .. }
                | Action::PointerPath { .. }
                | Action::Scroll { .. }
                | Action::Key { .. }
                | Action::KeyChord { .. }
                | Action::Text { .. }
                | Action::Accessibility { .. }
                | Action::ClipboardRead { .. }
                | Action::ClipboardWrite { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{
        LogicalDelta, LogicalPoint, LogicalRect, OutputId, OutputTransform, SourceSelector,
    };

    use super::*;

    struct SequencedExecutor;

    impl ActionExecutor for SequencedExecutor {
        fn execute(
            &self,
            action: &Action,
            _cancellation: &ExecutionToken,
        ) -> Result<ActionRoute, ProtocolError> {
            match action {
                Action::Capture { .. } => Ok(ActionRoute::Screencopy),
                _ => Err(ProtocolError {
                    code: ErrorCode::BackendFailure,
                    message: "deliberate test failure".into(),
                    retryable: false,
                }),
            }
        }
    }

    fn grounding() -> ObservationGrounding {
        ObservationGrounding {
            source: SourceSelector::Output {
                output_id: OutputId("DP-1".into()),
            },
            output_id: OutputId("DP-1".into()),
            output_logical_bounds: LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 1920.0,
                height: 1080.0,
            },
            observed_logical_bounds: LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 640.0,
                height: 480.0,
            },
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused_window: Some(WindowId("41".into())),
            cursor_position: None,
        }
    }

    fn output() -> OutputState {
        let grounding = grounding();
        OutputState {
            id: grounding.output_id,
            name: "DP-1".into(),
            logical_bounds: grounding.output_logical_bounds,
            pixel_size: None,
            scale: grounding.scale,
            transform: grounding.transform,
            focused: true,
        }
    }

    #[test]
    fn missing_observations_and_outputs_fail_distinctly() {
        assert_eq!(
            validate_grounding(None, &[output()], None, &[]),
            Err(GroundingFailure::UnknownObservation)
        );
        assert_eq!(
            validate_grounding(Some(&grounding()), &[], None, &[]),
            Err(GroundingFailure::TargetLost)
        );
    }

    #[test]
    fn any_coordinate_mapping_change_invalidates_the_observation() {
        let recorded = grounding();
        let mut moved = output();
        moved.logical_bounds.origin.x = 1920.0;
        assert_eq!(
            validate_grounding(Some(&recorded), &[moved], None, &[]),
            Err(GroundingFailure::GeometryChanged)
        );

        let mut rescaled = output();
        rescaled.scale = 2.0;
        assert_eq!(
            validate_grounding(Some(&recorded), &[rescaled], None, &[]),
            Err(GroundingFailure::GeometryChanged)
        );
    }

    #[test]
    fn application_input_requires_the_original_focus() {
        let action = Action::Scroll {
            delta: LogicalDelta { x: 0.0, y: 1.0 },
        };
        let changed_focus = WindowId("99".into());

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                Some(&changed_focus),
                &[action]
            ),
            Err(GroundingFailure::FocusChanged)
        );
    }

    #[test]
    fn niri_window_operations_do_not_invent_a_focus_precondition() {
        let action = Action::Window {
            window_id: WindowId("17".into()),
            operation: waldo_protocol::WindowOperation::Focus,
        };

        assert_eq!(
            validate_grounding(Some(&grounding()), &[output()], None, &[action]),
            Ok(())
        );
    }

    #[test]
    fn ordered_execution_stops_at_failure_and_marks_the_unattempted_tail() {
        let capture = Action::Capture {
            source: grounding().source,
        };
        let failing = Action::Scroll {
            delta: LogicalDelta { x: 0.0, y: 1.0 },
        };
        let cancellation = test_token();
        let execution = execute_batch(
            &SequencedExecutor,
            &[capture.clone(), failing, capture],
            &cancellation,
        );

        assert_eq!(execution.first_not_executed, Some(1));
        assert_eq!(execution.stop_reason, StopReason::ExecutionFailed);
        assert_eq!(execution.receipts[0].status, ReceiptStatus::Executed);
        assert_eq!(execution.receipts[0].route, Some(ActionRoute::Screencopy));
        assert_eq!(execution.receipts[1].status, ReceiptStatus::Failed);
        assert_eq!(execution.receipts[2].status, ReceiptStatus::NotExecuted);
    }

    #[test]
    fn empty_batches_complete_without_invented_receipts() {
        let execution = execute_batch(&SequencedExecutor, &[], &test_token());

        assert!(execution.receipts.is_empty());
        assert_eq!(execution.first_not_executed, None);
        assert_eq!(execution.stop_reason, StopReason::Completed);
    }

    fn test_token() -> ExecutionToken {
        let mut controller = waldo_controller::Controller::new(std::time::Duration::from_secs(30));
        let connection = controller.connect(waldo_protocol::ClientId::new("test").unwrap());
        controller
            .begin_execution(connection, std::time::Duration::ZERO)
            .unwrap()
            .cancellation()
            .clone()
    }

    #[test]
    fn absolute_pointer_targets_must_be_inside_the_observed_region() {
        let action = Action::PointerClick {
            position: LogicalPoint { x: 700.0, y: 20.0 },
            button: waldo_protocol::PointerButton::Primary,
            count: 1,
        };

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                Some(&WindowId("41".into())),
                &[action]
            ),
            Err(GroundingFailure::TargetOutsideObservation)
        );
    }

    #[test]
    fn an_unchanged_region_authorises_points_visible_inside_it() {
        let action = Action::PointerClick {
            position: LogicalPoint { x: 320.0, y: 240.0 },
            button: waldo_protocol::PointerButton::Primary,
            count: 1,
        };

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                Some(&WindowId("41".into())),
                &[action]
            ),
            Ok(())
        );
    }

    #[test]
    fn cursor_relative_actions_fail_closed_without_cursor_evidence() {
        let actions = [
            Action::PointerMoveRelative {
                delta: LogicalDelta { x: 1.0, y: 0.0 },
            },
            Action::Scroll {
                delta: LogicalDelta { x: 0.0, y: 1.0 },
            },
        ];

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                Some(&WindowId("41".into())),
                &actions
            ),
            Err(GroundingFailure::TargetOutsideObservation)
        );
    }

    #[test]
    fn preceding_absolute_motion_can_ground_later_relative_actions() {
        let actions = [
            Action::PointerMoveAbsolute {
                position: LogicalPoint { x: 100.0, y: 100.0 },
            },
            Action::PointerMoveRelative {
                delta: LogicalDelta { x: 10.0, y: 10.0 },
            },
            Action::PointerButton {
                button: waldo_protocol::PointerButton::Primary,
                state: waldo_protocol::InputState::Pressed,
            },
        ];

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                Some(&WindowId("41".into())),
                &actions
            ),
            Ok(())
        );
    }

    #[test]
    fn cancellation_marks_the_entire_unattempted_batch() {
        let token = test_token();
        token.cancel();
        let action = Action::Capture {
            source: grounding().source,
        };

        let execution = execute_batch(&SequencedExecutor, &[action], &token);

        assert_eq!(execution.stop_reason, StopReason::Cancelled);
        assert_eq!(execution.first_not_executed, Some(0));
        assert_eq!(execution.receipts[0].status, ReceiptStatus::NotExecuted);
    }
}
