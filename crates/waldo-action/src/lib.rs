//! Backend-independent safety checks for observation-grounded action batches.

use waldo_controller::{ExecutionToken, ObservationGrounding};
use waldo_protocol::{
    Action, ActionOutput, ActionReceipt, ActionRoute, ErrorCode, LogicalPoint, LogicalRect,
    OutputState, ProtocolError, ReceiptStatus, SourceSelector, StopReason, WindowId,
    WindowOperation, WindowState, WorkspaceOperation, WorkspaceState,
};

/// Successful backend execution with its route and optional produced value.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionExecution {
    pub route: ActionRoute,
    pub output: Option<ActionOutput>,
}

impl From<ActionRoute> for ActionExecution {
    fn from(route: ActionRoute) -> Self {
        Self {
            route,
            output: None,
        }
    }
}

/// The time stamp for a synthesised Wayland input event: milliseconds since
/// the Unix epoch, wrapped to the protocols' 32 bits. The virtual pointer and
/// keyboard share it so their events carry one clock.
pub fn input_event_time_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

/// Backend seam beneath the controller-owned ordering and safety policy.
pub trait ActionExecutor: Send + Sync {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure>;

    /// Whether this executor has initialised the named live backend route.
    fn supports(&self, _route: ActionRoute) -> bool {
        false
    }

    /// Payload-free reason a discovered route is temporarily blocked.
    fn limitation(&self, _route: ActionRoute) -> Option<String> {
        None
    }

    /// Payload-free note about a route that is still usable, such as an
    /// earlier write that is no longer offered.
    fn caveat(&self, _route: ActionRoute) -> Option<String> {
        None
    }

    /// Releases any explicitly held input, tears down its virtual devices and
    /// restores preserved desktop state. Returns a payload-free reason when
    /// cleanup left state unrestored, so the boundary that asked for it can
    /// report the failure.
    fn release_all(&self) -> Option<String> {
        None
    }
}

/// A backend failure with an honest statement about whether mutation may have occurred.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionExecutionFailure {
    NotExecuted {
        error: ProtocolError,
    },
    Indeterminate {
        route: ActionRoute,
        error: ProtocolError,
    },
}

impl ActionExecutionFailure {
    pub fn not_executed(error: ProtocolError) -> Self {
        Self::NotExecuted { error }
    }

    pub fn indeterminate(route: ActionRoute, error: ProtocolError) -> Self {
        Self::Indeterminate { route, error }
    }

    pub fn error(&self) -> &ProtocolError {
        match self {
            Self::NotExecuted { error } | Self::Indeterminate { error, .. } => error,
        }
    }
}

/// Explicit fail-closed executor used until a live action route passes its gate.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableActionExecutor;

impl ActionExecutor for UnavailableActionExecutor {
    fn execute(
        &self,
        _action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        Err(ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: "no desktop action backend is initialised".into(),
            retryable: false,
        }))
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
                predicate_result: None,
                output: None,
            }));
            break;
        }
        match executor.execute(action, cancellation) {
            Ok(execution) => receipts.push(ActionReceipt {
                index,
                status: ReceiptStatus::Executed,
                route: Some(execution.route),
                error: None,
                predicate_result: matches!(action, Action::Wait { .. }).then_some(true),
                output: execution.output,
            }),
            Err(failure) => {
                let (status, route, error) = match failure {
                    ActionExecutionFailure::NotExecuted { error } => {
                        first_not_executed = Some(index);
                        (ReceiptStatus::Failed, None, error)
                    }
                    ActionExecutionFailure::Indeterminate { route, error } => {
                        first_not_executed = (index + 1 < actions.len()).then_some(index + 1);
                        (ReceiptStatus::Indeterminate, Some(route), error)
                    }
                };
                receipts.push(ActionReceipt {
                    index,
                    status,
                    route,
                    predicate_result: (error.code == ErrorCode::WaitFailed).then_some(false),
                    error: Some(error),
                    output: None,
                });
                receipts.extend((index + 1..actions.len()).map(|index| ActionReceipt {
                    index,
                    status: ReceiptStatus::NotExecuted,
                    route: None,
                    error: None,
                    predicate_result: None,
                    output: None,
                }));
                break;
            }
        }
    }
    let completed = receipts.len() == actions.len()
        && receipts
            .iter()
            .all(|receipt| receipt.status == ReceiptStatus::Executed);
    let stop_reason = if completed {
        StopReason::Completed
    } else if cancellation.is_cancelled() {
        StopReason::Cancelled
    } else if let Some(error) = receipts.iter().find_map(|receipt| receipt.error.as_ref()) {
        match error.code {
            ErrorCode::WaitFailed => StopReason::WaitFailed,
            ErrorCode::StaleObservation => StopReason::StaleObservation,
            ErrorCode::TargetLost => StopReason::TargetLost,
            ErrorCode::GeometryChanged => StopReason::GeometryChanged,
            ErrorCode::FocusChanged => StopReason::FocusChanged,
            _ => StopReason::ExecutionFailed,
        }
    } else {
        StopReason::ExecutionFailed
    };
    BatchExecution {
        receipts,
        first_not_executed,
        stop_reason,
    }
}

/// A mechanical reason an observation can no longer authorise its batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroundingFailure {
    UnknownObservation,
    TargetLost,
    GeometryChanged,
    FocusChanged,
    CrossOutputDragUnsupported,
    MissingCursorGrounding,
    TargetOutsideObservation,
}

impl GroundingFailure {
    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::UnknownObservation => ErrorCode::StaleObservation,
            Self::TargetLost => ErrorCode::TargetLost,
            Self::GeometryChanged => ErrorCode::GeometryChanged,
            Self::FocusChanged => ErrorCode::FocusChanged,
            Self::CrossOutputDragUnsupported => ErrorCode::UnsupportedCapability,
            Self::MissingCursorGrounding => ErrorCode::InvalidRequest,
            Self::TargetOutsideObservation => ErrorCode::InvalidRequest,
        }
    }

    pub fn protocol_error(self) -> ProtocolError {
        let message = match self {
            Self::UnknownObservation => "the cited observation is no longer retained",
            Self::TargetLost => "an observed action target or destination is no longer available",
            Self::GeometryChanged => {
                "the observed output geometry or coordinate transform has changed"
            }
            Self::FocusChanged => "foreground focus changed after the cited observation",
            Self::CrossOutputDragUnsupported => {
                "cross-output drag is unsupported and was refused before pointer movement or button press"
            }
            Self::MissingCursorGrounding => {
                "the action requires a cursor position, but the cited observation and preceding executed actions provide none"
            }
            Self::TargetOutsideObservation => {
                "an absolute pointer target lies outside the pixels in the cited observation"
            }
        };
        ProtocolError {
            code: self.error_code(),
            message: message.into(),
            retryable: self != Self::CrossOutputDragUnsupported,
        }
    }
}

/// Revalidates facts that can redirect an otherwise valid action.
pub fn validate_grounding(
    recorded: Option<&ObservationGrounding>,
    outputs: &[OutputState],
    windows: &[WindowState],
    workspaces: &[WorkspaceState],
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
    if actions.iter().any(|action| match action {
        Action::Window { window_id, .. } => !windows.iter().any(|window| &window.id == window_id),
        Action::Workspace { workspace_id, .. } => !workspaces
            .iter()
            .any(|workspace| &workspace.id == workspace_id),
        _ => false,
    }) {
        return Err(GroundingFailure::TargetLost);
    }
    if actions.iter().any(|action| match action {
        Action::Window {
            operation: WindowOperation::MoveToWorkspace { workspace_id },
            ..
        } => !workspaces
            .iter()
            .any(|candidate| candidate.id == *workspace_id),
        Action::Window {
            operation: WindowOperation::MoveToOutput { output },
            ..
        }
        | Action::Workspace {
            operation: WorkspaceOperation::MoveToOutput { output },
            ..
        } => !outputs
            .iter()
            .any(|candidate| candidate.id.0 == *output || candidate.name == *output),
        _ => false,
    }) {
        return Err(GroundingFailure::TargetLost);
    }
    if let SourceSelector::Window { window_id } = &recorded.source {
        let window = windows
            .iter()
            .find(|window| &window.id == window_id)
            .ok_or(GroundingFailure::TargetLost)?;
        if window.output_id.as_ref() != Some(&recorded.output_id)
            || window.logical_bounds != Some(recorded.observed_logical_bounds)
        {
            return Err(GroundingFailure::GeometryChanged);
        }
    }
    if actions_require_stable_focus(actions) && focused_window != recorded.focused_window.as_ref() {
        return Err(GroundingFailure::FocusChanged);
    }
    if contains_cross_output_drag(actions, outputs) {
        return Err(GroundingFailure::CrossOutputDragUnsupported);
    }
    pointer_targets_within(
        actions,
        recorded.observed_logical_bounds,
        recorded.cursor_position,
    )?;
    Ok(())
}

fn contains_cross_output_drag(actions: &[Action], outputs: &[OutputState]) -> bool {
    actions.iter().any(|action| {
        let Action::PointerDrag { from, to, .. } = action else {
            return false;
        };
        let from_output = outputs
            .iter()
            .find(|output| point_within(*from, output.logical_bounds));
        let to_output = outputs
            .iter()
            .find(|output| point_within(*to, output.logical_bounds));
        matches!((from_output, to_output), (Some(from), Some(to)) if from.id != to.id)
    })
}

fn pointer_targets_within(
    actions: &[Action],
    bounds: LogicalRect,
    mut cursor: Option<LogicalPoint>,
) -> Result<(), GroundingFailure> {
    for action in actions {
        match action {
            Action::PointerMoveAbsolute { position } | Action::PointerClick { position, .. } => {
                if !point_within(*position, bounds) {
                    return Err(GroundingFailure::TargetOutsideObservation);
                }
            }
            Action::PointerDrag { from, to, .. } => {
                if !point_within(*from, bounds) || !point_within(*to, bounds) {
                    return Err(GroundingFailure::TargetOutsideObservation);
                }
            }
            Action::PointerPath { points, .. } => {
                if !points
                    .iter()
                    .copied()
                    .all(|point| point_within(point, bounds))
                {
                    return Err(GroundingFailure::TargetOutsideObservation);
                }
            }
            Action::PointerMoveRelative { .. } => {
                let Some(destination) = cursor_after(action, cursor) else {
                    return Err(GroundingFailure::MissingCursorGrounding);
                };
                if !point_within(destination, bounds) {
                    return Err(GroundingFailure::TargetOutsideObservation);
                }
            }
            Action::PointerButton { .. } | Action::Scroll { .. } => {
                let Some(position) = cursor else {
                    return Err(GroundingFailure::MissingCursorGrounding);
                };
                if !point_within(position, bounds) {
                    return Err(GroundingFailure::TargetOutsideObservation);
                }
            }
            _ => {}
        }
        cursor = cursor_after(action, cursor);
    }
    Ok(())
}

fn point_within(point: LogicalPoint, bounds: LogicalRect) -> bool {
    bounds.contains(point)
}

/// Where the pointer is after an action, given where it was before: the
/// one cursor transition that both grounding validation and the executed
/// batch's bookkeeping follow. Relative motion from an unknown position
/// leaves it unknown.
pub fn cursor_after(action: &Action, cursor: Option<LogicalPoint>) -> Option<LogicalPoint> {
    match action {
        Action::PointerMoveAbsolute { position } | Action::PointerClick { position, .. } => {
            Some(*position)
        }
        Action::PointerDrag { to, .. } => Some(*to),
        Action::PointerPath { points, .. } => points.last().copied().or(cursor),
        Action::PointerMoveRelative { delta } => cursor.map(|current| LogicalPoint {
            x: current.x + delta.x,
            y: current.y + delta.y,
        }),
        _ => cursor,
    }
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
                | Action::ClipboardWriteOffer { .. }
                | Action::ClipboardPreserve
                | Action::ClipboardRestore { .. }
                | Action::ClipboardKeep { .. }
        )
    })
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{
        LogicalDelta, LogicalPoint, LogicalRect, OutputId, OutputTransform, SourceSelector,
        WindowState,
    };

    use super::*;

    struct SequencedExecutor;

    struct IndeterminateExecutor;

    struct SuccessfulAfterCancellationExecutor;

    impl ActionExecutor for SequencedExecutor {
        fn execute(
            &self,
            action: &Action,
            _cancellation: &ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            match action {
                Action::Capture { .. } => Ok(ActionRoute::Screencopy.into()),
                _ => Err(ActionExecutionFailure::not_executed(ProtocolError {
                    code: ErrorCode::BackendFailure,
                    message: "deliberate test failure".into(),
                    retryable: false,
                })),
            }
        }
    }

    impl ActionExecutor for IndeterminateExecutor {
        fn execute(
            &self,
            _action: &Action,
            _cancellation: &ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            Err(ActionExecutionFailure::indeterminate(
                ActionRoute::NiriIpc,
                ProtocolError {
                    code: ErrorCode::BackendFailure,
                    message: "acknowledgement lost after dispatch".into(),
                    retryable: true,
                },
            ))
        }
    }

    impl ActionExecutor for SuccessfulAfterCancellationExecutor {
        fn execute(
            &self,
            _action: &Action,
            cancellation: &ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            cancellation.cancel();
            Ok(ActionRoute::NiriIpc.into())
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
            include_accessibility: false,
            cursor_position: None,
            frame_fingerprint: 0,
            application_frame_fingerprint: 0,
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

    fn window(id: &str) -> WindowState {
        WindowState {
            id: WindowId(id.into()),
            title: None,
            application_id: None,
            pid: None,
            logical_bounds: None,
            is_floating: true,
            capture: waldo_protocol::WindowCaptureCapability {
                available: true,
                limitation: None,
            },
            focused: false,
            workspace_id: None,
            output_id: None,
        }
    }

    fn workspace(name: &str) -> WorkspaceState {
        WorkspaceState {
            id: waldo_protocol::WorkspaceId::new(3),
            index: 3,
            name: Some(name.into()),
            output_id: Some(OutputId("DP-1".into())),
            active: true,
            focused: true,
        }
    }

    #[test]
    fn missing_observations_and_outputs_fail_distinctly() {
        assert_eq!(
            validate_grounding(None, &[output()], &[], &[], None, &[]),
            Err(GroundingFailure::UnknownObservation)
        );
        assert_eq!(
            validate_grounding(Some(&grounding()), &[], &[], &[], None, &[]),
            Err(GroundingFailure::TargetLost)
        );
    }

    #[test]
    fn any_coordinate_mapping_change_invalidates_the_observation() {
        let recorded = grounding();
        let mut moved = output();
        moved.logical_bounds.origin.x = 1920.0;
        assert_eq!(
            validate_grounding(Some(&recorded), &[moved], &[], &[], None, &[]),
            Err(GroundingFailure::GeometryChanged)
        );

        let mut rescaled = output();
        rescaled.scale = 2.0;
        assert_eq!(
            validate_grounding(Some(&recorded), &[rescaled], &[], &[], None, &[]),
            Err(GroundingFailure::GeometryChanged)
        );
    }

    #[test]
    fn cross_output_drag_is_refused_before_backend_dispatch() {
        let recorded = grounding();
        let mut second_output = output();
        second_output.id = OutputId("DP-2".into());
        second_output.name = "DP-2".into();
        second_output.logical_bounds.origin.x = 1920.0;
        let drag = Action::PointerDrag {
            from: LogicalPoint {
                x: 1800.0,
                y: 500.0,
            },
            to: LogicalPoint {
                x: 2100.0,
                y: 500.0,
            },
            button: waldo_protocol::PointerButton::Primary,
            duration_ms: 250,
        };

        let failure = validate_grounding(
            Some(&recorded),
            &[output(), second_output],
            &[],
            &[],
            recorded.focused_window.as_ref(),
            &[drag],
        )
        .unwrap_err();

        assert_eq!(failure, GroundingFailure::CrossOutputDragUnsupported);
        let error = failure.protocol_error();
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("before pointer movement or button press")
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
                &[],
                &[],
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
            validate_grounding(
                Some(&grounding()),
                &[output()],
                &[window("17")],
                &[],
                None,
                &[action]
            ),
            Ok(())
        );
    }

    #[test]
    fn niri_window_operations_require_the_target_to_still_exist() {
        let action = Action::Window {
            window_id: WindowId("17".into()),
            operation: waldo_protocol::WindowOperation::Focus,
        };

        assert_eq!(
            validate_grounding(Some(&grounding()), &[output()], &[], &[], None, &[action]),
            Err(GroundingFailure::TargetLost)
        );
    }

    #[test]
    fn direct_window_observations_revalidate_exact_geometry() {
        let mut recorded = grounding();
        recorded.source = SourceSelector::Window {
            window_id: WindowId("17".into()),
        };
        let mut current = window("17");
        current.logical_bounds = Some(recorded.observed_logical_bounds);
        current.output_id = Some(recorded.output_id.clone());

        assert_eq!(
            validate_grounding(
                Some(&recorded),
                &[output()],
                &[current.clone()],
                &[],
                recorded.focused_window.as_ref(),
                &[],
            ),
            Ok(())
        );

        current.logical_bounds.as_mut().unwrap().origin.x += 1.0;
        assert_eq!(
            validate_grounding(
                Some(&recorded),
                &[output()],
                &[current],
                &[],
                recorded.focused_window.as_ref(),
                &[],
            ),
            Err(GroundingFailure::GeometryChanged)
        );
    }

    #[test]
    fn workspace_operations_require_the_stable_workspace_identity() {
        let action = Action::Workspace {
            workspace_id: waldo_protocol::WorkspaceId::new(99),
            operation: WorkspaceOperation::Focus,
        };

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                &[],
                &[workspace("research")],
                None,
                &[action],
            ),
            Err(GroundingFailure::TargetLost)
        );
    }

    #[test]
    fn niri_window_moves_require_the_destination_to_still_exist() {
        let to_workspace = Action::Window {
            window_id: WindowId("17".into()),
            operation: WindowOperation::MoveToWorkspace {
                workspace_id: waldo_protocol::WorkspaceId::new(99),
            },
        };
        let to_output = Action::Window {
            window_id: WindowId("17".into()),
            operation: WindowOperation::MoveToOutput {
                output: "DP-2".into(),
            },
        };

        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                &[window("17")],
                &[workspace("research")],
                None,
                &[to_workspace]
            ),
            Err(GroundingFailure::TargetLost)
        );
        assert_eq!(
            validate_grounding(
                Some(&grounding()),
                &[output()],
                &[window("17")],
                &[workspace("research")],
                None,
                &[to_output]
            ),
            Err(GroundingFailure::TargetLost)
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
    fn uncertain_dispatch_is_distinct_from_the_definitely_unattempted_tail() {
        let action = Action::Window {
            window_id: WindowId("17".into()),
            operation: WindowOperation::Focus,
        };
        let execution = execute_batch(
            &IndeterminateExecutor,
            &[action.clone(), action],
            &test_token(),
        );

        assert_eq!(execution.stop_reason, StopReason::ExecutionFailed);
        assert_eq!(execution.first_not_executed, Some(1));
        assert_eq!(execution.receipts[0].status, ReceiptStatus::Indeterminate);
        assert_eq!(execution.receipts[0].route, Some(ActionRoute::NiriIpc));
        assert_eq!(execution.receipts[1].status, ReceiptStatus::NotExecuted);
    }

    #[test]
    fn acknowledged_final_action_completes_even_if_cancellation_arrived_in_flight() {
        let action = Action::Window {
            window_id: WindowId("17".into()),
            operation: WindowOperation::Focus,
        };
        let execution = execute_batch(
            &SuccessfulAfterCancellationExecutor,
            &[action],
            &test_token(),
        );

        assert_eq!(execution.stop_reason, StopReason::Completed);
        assert_eq!(execution.first_not_executed, None);
        assert_eq!(execution.receipts[0].status, ReceiptStatus::Executed);
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
        let connection = controller.connect();
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
                &[],
                &[],
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
                &[],
                &[],
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
                &[],
                &[],
                Some(&WindowId("41".into())),
                &actions
            ),
            Err(GroundingFailure::MissingCursorGrounding)
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
                &[],
                &[],
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
