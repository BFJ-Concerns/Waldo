use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use waldo_accessibility::AccessibilityBackend;
use waldo_action::{
    ActionExecution, ActionExecutionFailure, ActionExecutor, GroundingFailure, validate_grounding,
};
use waldo_controller::{ExecutionToken, ObservationGrounding};
use waldo_niri::SessionInspector;
use waldo_protocol::{
    Action, ActionRoute, ErrorCode, ObservationId, Predicate, ProtocolError, SourceSelector,
    WindowOperation, WorkspaceOperation,
};
use waldo_screencopy::ScreenCapturer;

use crate::observe::{capture_frame, frame_fingerprint};

const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Adds controller-owned wait and capture semantics to the desktop actuators.
pub(crate) struct BatchActionExecutor {
    desktop: Arc<dyn ActionExecutor>,
    inspector: Arc<dyn SessionInspector>,
    capturer: Arc<dyn ScreenCapturer>,
    accessibility: Option<Arc<AccessibilityBackend>>,
    grounding: ObservationGrounding,
    referenced_frames: BTreeMap<ObservationId, ObservationGrounding>,
    invalidated_grounding: Mutex<Option<GroundingFailure>>,
}

impl BatchActionExecutor {
    pub(crate) fn new(
        desktop: Arc<dyn ActionExecutor>,
        inspector: Arc<dyn SessionInspector>,
        capturer: Arc<dyn ScreenCapturer>,
        accessibility: Option<Arc<AccessibilityBackend>>,
        grounding: ObservationGrounding,
        referenced_frames: BTreeMap<ObservationId, ObservationGrounding>,
    ) -> Self {
        Self {
            desktop,
            inspector,
            capturer,
            accessibility,
            grounding,
            referenced_frames,
            invalidated_grounding: Mutex::new(None),
        }
    }

    fn wait(
        &self,
        predicate: &Predicate,
        timeout_ms: u32,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if cancellation.is_cancelled() {
                return Err(ActionExecutionFailure::not_executed(cancelled()));
            }
            if self
                .predicate(predicate)
                .map_err(ActionExecutionFailure::not_executed)?
            {
                return Ok(ActionRoute::ControllerWait.into());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ActionExecutionFailure::not_executed(ProtocolError {
                    code: ErrorCode::WaitFailed,
                    message: format!(
                        "wait predicate was still false after {timeout_ms} milliseconds"
                    ),
                    retryable: true,
                }));
            }
            thread::sleep(WAIT_POLL_INTERVAL.min(deadline - now));
        }
    }

    fn predicate(&self, predicate: &Predicate) -> Result<bool, ProtocolError> {
        match predicate {
            Predicate::FrameChanged { from } => {
                let grounding = self
                    .referenced_frames
                    .get(from)
                    .ok_or_else(|| ProtocolError {
                        code: ErrorCode::StaleObservation,
                        message: "wait references an observation which is no longer retained"
                            .into(),
                        retryable: true,
                    })?;
                let frame = capture_frame(
                    &grounding.source,
                    self.inspector.as_ref(),
                    self.capturer.as_ref(),
                    false,
                )?;
                Ok(frame_fingerprint(&frame.capture.png) != grounding.frame_fingerprint)
            }
            Predicate::WindowExists { window_id } => Ok(self
                .snapshot_windows()?
                .windows
                .iter()
                .any(|window| &window.id == window_id)),
            Predicate::FocusedWindow { window_id } => {
                Ok(self.snapshot_windows()?.focused_window.as_ref() == Some(window_id))
            }
            Predicate::ProcessExited { pid } => {
                if *pid == 0 {
                    return Err(ProtocolError {
                        code: ErrorCode::InvalidRequest,
                        message: "process wait requires a non-zero PID".into(),
                        retryable: false,
                    });
                }
                Ok(!Path::new(&format!("/proc/{pid}")).exists())
            }
            Predicate::ElementExists { element } => self.accessibility()?.element_exists(element),
            Predicate::ElementState {
                element,
                state,
                present,
            } => Ok(self
                .accessibility()?
                .element_state(element, state)?
                .is_some_and(|actual| actual == *present)),
        }
    }

    fn accessibility(&self) -> Result<&AccessibilityBackend, ProtocolError> {
        self.accessibility.as_deref().ok_or_else(|| ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: "AT-SPI backend is unavailable for accessibility predicates".into(),
            retryable: false,
        })
    }

    fn snapshot_windows(&self) -> Result<waldo_niri::NiriSnapshot, ProtocolError> {
        self.inspector.inspect(true).map_err(|error| ProtocolError {
            code: ErrorCode::BackendFailure,
            message: format!("could not inspect Niri while evaluating a wait: {error}"),
            retryable: true,
        })
    }

    fn capture(&self, source: &SourceSelector) -> Result<ActionExecution, ActionExecutionFailure> {
        capture_frame(
            source,
            self.inspector.as_ref(),
            self.capturer.as_ref(),
            false,
        )
        .map(|_| ActionRoute::Screencopy.into())
        .map_err(ActionExecutionFailure::not_executed)
    }

    fn validate_before_dispatch(&self, action: &Action) -> Result<(), ActionExecutionFailure> {
        if action_requires_stable_grounding(action) {
            let invalidated = *self.invalidated_grounding.lock().map_err(|_| {
                ActionExecutionFailure::not_executed(ProtocolError {
                    code: ErrorCode::Internal,
                    message: "batch grounding state is unavailable".into(),
                    retryable: true,
                })
            })?;
            if let Some(failure) = invalidated {
                return Err(ActionExecutionFailure::not_executed(
                    failure.protocol_error(),
                ));
            }
        }
        let include_windows = matches!(action, Action::Window { .. } | Action::Workspace { .. })
            || matches!(&self.grounding.source, SourceSelector::Window { .. });
        let snapshot = self.inspector.inspect(include_windows).map_err(|error| {
            ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::BackendFailure,
                message: format!("could not revalidate desktop state before action: {error}"),
                retryable: true,
            })
        })?;
        validate_grounding(
            Some(&self.grounding),
            &snapshot.outputs,
            &snapshot.windows,
            &snapshot.workspaces,
            snapshot.focused_window.as_ref(),
            std::slice::from_ref(action),
        )
        .map_err(|failure| ActionExecutionFailure::not_executed(failure.protocol_error()))
    }

    fn record_grounding_invalidation(&self, action: &Action) -> Result<(), ActionExecutionFailure> {
        let Some(failure) = action_grounding_invalidation(action) else {
            return Ok(());
        };
        *self.invalidated_grounding.lock().map_err(|_| {
            ActionExecutionFailure::indeterminate(
                ActionRoute::NiriIpc,
                ProtocolError {
                    code: ErrorCode::Internal,
                    message: "batch grounding state is unavailable after desktop mutation".into(),
                    retryable: true,
                },
            )
        })? = Some(failure);
        Ok(())
    }
}

impl ActionExecutor for BatchActionExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        match action {
            Action::Wait {
                predicate,
                timeout_ms,
            } => self.wait(predicate, *timeout_ms, cancellation),
            Action::Capture { source } => {
                if cancellation.is_cancelled() {
                    Err(ActionExecutionFailure::not_executed(cancelled()))
                } else {
                    self.capture(source)
                }
            }
            _ => {
                self.validate_before_dispatch(action)?;
                let execution = self.desktop.execute(action, cancellation)?;
                self.record_grounding_invalidation(action)?;
                Ok(execution)
            }
        }
    }

    fn supports(&self, route: ActionRoute) -> bool {
        matches!(route, ActionRoute::ControllerWait | ActionRoute::Screencopy)
            || self.desktop.supports(route)
    }

    fn release_all(&self) {
        self.desktop.release_all();
    }
}

fn action_requires_stable_grounding(action: &Action) -> bool {
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
    )
}

fn action_grounding_invalidation(action: &Action) -> Option<GroundingFailure> {
    match action {
        Action::Window {
            operation: WindowOperation::Focus,
            ..
        }
        | Action::Workspace {
            operation: WorkspaceOperation::Focus,
            ..
        } => Some(GroundingFailure::FocusChanged),
        Action::Window {
            operation: WindowOperation::Close,
            ..
        } => Some(GroundingFailure::TargetLost),
        Action::Window {
            operation:
                WindowOperation::MoveToWorkspace { .. }
                | WindowOperation::MoveToOutput { .. }
                | WindowOperation::SetSize { .. },
            ..
        }
        | Action::Workspace {
            operation: WorkspaceOperation::MoveToOutput { .. },
            ..
        } => Some(GroundingFailure::GeometryChanged),
        _ => None,
    }
}

fn cancelled() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Cancelled,
        message: "the action batch was cancelled during a controller wait".into(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use waldo_action::execute_batch;
    use waldo_niri::{InspectionError, NiriSnapshot};
    use waldo_protocol::{
        ClientId, LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform, PointerButton,
        ReceiptStatus, WindowId, WindowState,
    };
    use waldo_screencopy::UnavailableCapturer;

    use super::*;

    struct FixedInspector(NiriSnapshot);

    impl SessionInspector for FixedInspector {
        fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, InspectionError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct CountingExecutor(AtomicUsize);

    impl ActionExecutor for CountingExecutor {
        fn execute(
            &self,
            _action: &Action,
            _cancellation: &ExecutionToken,
        ) -> Result<ActionExecution, ActionExecutionFailure> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ActionRoute::NiriIpc.into())
        }
    }

    #[test]
    fn compositor_mutation_invalidates_later_application_input_without_waiting_for_ipc_state() {
        let desktop = Arc::new(CountingExecutor::default());
        let executor = BatchActionExecutor::new(
            desktop.clone(),
            Arc::new(FixedInspector(snapshot())),
            Arc::new(UnavailableCapturer),
            None,
            grounding(),
            BTreeMap::new(),
        );
        let actions = [
            Action::Window {
                window_id: WindowId("17".into()),
                operation: WindowOperation::SetSize {
                    width: 420,
                    height: 380,
                },
            },
            Action::PointerClick {
                position: LogicalPoint { x: 100.0, y: 100.0 },
                button: PointerButton::Primary,
                count: 1,
            },
        ];

        let execution = execute_batch(&executor, &actions, &test_token());

        assert_eq!(desktop.0.load(Ordering::Relaxed), 1);
        assert_eq!(execution.receipts[0].status, ReceiptStatus::Executed);
        assert_eq!(execution.receipts[1].status, ReceiptStatus::Failed);
        assert_eq!(
            execution.stop_reason,
            waldo_protocol::StopReason::GeometryChanged
        );
        assert_eq!(
            execution.receipts[1].error.as_ref().map(|error| error.code),
            Some(ErrorCode::GeometryChanged)
        );
        assert_eq!(execution.first_not_executed, Some(1));
    }

    fn grounding() -> ObservationGrounding {
        ObservationGrounding {
            source: SourceSelector::Output {
                output_id: OutputId("DP-1".into()),
            },
            output_id: OutputId("DP-1".into()),
            output_logical_bounds: bounds(1920.0, 1080.0),
            observed_logical_bounds: bounds(1920.0, 1080.0),
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused_window: Some(WindowId("17".into())),
            include_accessibility: false,
            cursor_position: Some(LogicalPoint { x: 10.0, y: 10.0 }),
            frame_fingerprint: 0,
        }
    }

    fn snapshot() -> NiriSnapshot {
        let mut snapshot = NiriSnapshot::unavailable("test fixture");
        snapshot.outputs = vec![OutputState {
            id: OutputId("DP-1".into()),
            name: "DP-1".into(),
            logical_bounds: bounds(1920.0, 1080.0),
            pixel_size: None,
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused: true,
        }];
        snapshot.windows = vec![WindowState {
            id: WindowId("17".into()),
            title: None,
            application_id: None,
            pid: None,
            logical_bounds: None,
            focused: true,
            workspace_id: None,
            output_id: Some(OutputId("DP-1".into())),
        }];
        snapshot.focused_window = Some(WindowId("17".into()));
        snapshot
    }

    fn bounds(width: f64, height: f64) -> LogicalRect {
        LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width,
            height,
        }
    }

    fn test_token() -> ExecutionToken {
        let mut controller = waldo_controller::Controller::new(Duration::from_secs(30));
        let connection = controller.connect(ClientId::new("test").unwrap());
        controller
            .begin_execution(connection, Duration::ZERO)
            .unwrap()
            .cancellation()
            .clone()
    }
}
