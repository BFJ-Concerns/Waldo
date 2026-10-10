use std::{collections::BTreeMap, fs, io, path::Path, sync::Arc};

use waldo_niri::NiriSnapshot;
use waldo_protocol::{
    AccessibilityNode, ConnectionId, ElementHandle, ErrorCode, ObservationId, Predicate,
    ProtocolError, SourceSelector, WindowId,
};

use crate::{observe::CapturedObservation, server::ServiceState};

/// Predicates validated before mutation with any retained frame evidence attached.
pub(crate) struct VerificationPlan {
    predicates: Vec<Predicate>,
    prior_frames: BTreeMap<ObservationId, u64>,
}

impl VerificationPlan {
    /// Checks every predicate against the observation the batch will return:
    /// `result_source` is what that observation captures, and
    /// `include_accessibility` whether it carries semantic evidence.
    pub(crate) async fn prepare(
        predicates: Vec<Predicate>,
        result_source: &SourceSelector,
        include_accessibility: bool,
        state: &ServiceState,
        connection_id: ConnectionId,
    ) -> Result<Self, ProtocolError> {
        let controller = state.controller.lock().await;
        let mut prior_frames = BTreeMap::new();
        for predicate in &predicates {
            match predicate {
                Predicate::FrameChanged { from } => {
                    let grounding = controller.observation(connection_id, *from).ok_or_else(
                        || ProtocolError {
                            code: ErrorCode::StaleObservation,
                            message: format!(
                                "verification observation {} is not retained for this connection",
                                from.get()
                            ),
                            retryable: true,
                        },
                    )?;
                    if &grounding.source != result_source {
                        return Err(ProtocolError::invalid_request(
                            "frame-change verification must compare the source the batch's resulting observation captures",
                        ));
                    }
                    prior_frames.insert(*from, grounding.application_frame_fingerprint);
                }
                Predicate::ElementExists { .. } | Predicate::ElementState { .. } => {
                    if let Predicate::ElementState { state, .. } = predicate {
                        waldo_accessibility::require_known_state(state)?;
                    }
                    if !include_accessibility {
                        return Err(ProtocolError::invalid_request(
                            "element verification needs accessibility evidence; act on an observation made with include_accessibility",
                        ));
                    }
                }
                Predicate::ProcessExited { pid } => validate_pid(*pid)?,
                Predicate::WindowExists { .. } | Predicate::FocusedWindow { .. } => {}
            }
        }
        drop(controller);
        Ok(Self {
            predicates,
            prior_frames,
        })
    }

    pub(crate) async fn evaluate(
        &self,
        captured: &CapturedObservation,
        state: &ServiceState,
    ) -> Result<Vec<bool>, ProtocolError> {
        if self.predicates.is_empty() {
            return Ok(Vec::new());
        }
        let observation = &captured.observation;
        let needs_semantics = self.predicates.iter().any(|predicate| {
            matches!(
                predicate,
                Predicate::ElementExists { .. } | Predicate::ElementState { .. }
            )
        });
        if needs_semantics && !observation.accessibility_available {
            return Err(ProtocolError::backend_failure(
                format!(
                    "element verification could not be measured: {}",
                    observation
                        .accessibility_limitation
                        .as_deref()
                        .unwrap_or("the resulting observation has no accessibility evidence")
                ),
                true,
            ));
        }
        let needs_windows = self.predicates.iter().any(|predicate| {
            matches!(
                predicate,
                Predicate::WindowExists { .. } | Predicate::FocusedWindow { .. }
            )
        });
        let snapshot = if needs_windows {
            let inspector = Arc::clone(&state.inspector);
            Some(
                tokio::task::spawn_blocking(move || inspector.inspect(true))
                    .await
                    .map_err(|error| verification_backend(error.to_string()))?
                    .map_err(|error| verification_backend(error.to_string()))?,
            )
        } else {
            None
        };
        let windows = || {
            snapshot
                .as_ref()
                .expect("window predicates requested an inspection")
        };

        self.predicates
            .iter()
            .map(|predicate| match predicate {
                Predicate::FrameChanged { from } => Ok(self
                    .prior_frames
                    .get(from)
                    .is_some_and(|prior| *prior != captured.application_frame_fingerprint)),
                Predicate::WindowExists { window_id } => Ok(window_exists(windows(), window_id)),
                Predicate::FocusedWindow { window_id } => Ok(window_focused(windows(), window_id)),
                Predicate::ProcessExited { pid } => process_exited(*pid),
                Predicate::ElementExists { element } => {
                    Ok(observed_element(captured, element)?.is_some())
                }
                Predicate::ElementState {
                    element,
                    state,
                    present,
                } => Ok(observed_element(captured, element)?.is_some_and(|node| {
                    node.states.iter().any(|actual| actual == state) == *present
                })),
            })
            .collect()
    }
}

/// The resulting observation's node for `element`; absence counts only when
/// the snapshot covered the whole tree, since a cut-short tree may have
/// omitted it.
fn observed_element<'a>(
    captured: &'a CapturedObservation,
    element: &ElementHandle,
) -> Result<Option<&'a AccessibilityNode>, ProtocolError> {
    let node = captured
        .observation
        .accessibility
        .iter()
        .find(|node| &node.handle == element);
    if node.is_none() && !captured.accessibility_complete {
        return Err(verification_backend(format!(
            "element {} is unmeasurable: the accessibility tree was cut short before it",
            element.0
        )));
    }
    Ok(node)
}

/// The predicate semantics waits and post-action verification share; each
/// acquires its own evidence.
pub(crate) fn window_exists(snapshot: &NiriSnapshot, window_id: &WindowId) -> bool {
    snapshot
        .windows
        .iter()
        .any(|window| &window.id == window_id)
}

pub(crate) fn window_focused(snapshot: &NiriSnapshot, window_id: &WindowId) -> bool {
    snapshot.focused_window.as_ref() == Some(window_id)
}

pub(crate) fn process_exited(pid: u32) -> Result<bool, ProtocolError> {
    validate_pid(pid)?;
    process_directory_absent(Path::new("/proc"), pid)
}

/// A PID's directory missing from a readable process filesystem means the
/// process exited; any other failure to inspect it measures nothing.
fn process_directory_absent(proc_root: &Path, pid: u32) -> Result<bool, ProtocolError> {
    fs::metadata(proc_root).map_err(|error| {
        verification_backend(format!(
            "cannot inspect {} for process {pid}: {error}",
            proc_root.display()
        ))
    })?;
    match fs::metadata(proc_root.join(pid.to_string())) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(verification_backend(format!(
            "cannot inspect process {pid}: {error}"
        ))),
    }
}

fn validate_pid(pid: u32) -> Result<(), ProtocolError> {
    if pid == 0 {
        return Err(ProtocolError::invalid_request(
            "a process predicate requires a non-zero PID",
        ));
    }
    Ok(())
}

pub(crate) fn failed_verification() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::VerificationFailed,
        message: "one or more post-action verification predicates were false".into(),
        retryable: true,
    }
}

fn verification_backend(message: String) -> ProtocolError {
    ProtocolError::backend_failure(
        format!("could not inspect post-action verification state: {message}"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use waldo_protocol::{
        AccessibilityNode, CaptureProvenance, CaptureRoute, CoordinateMapping, ElementHandle,
        ErrorCode, FocusState, ImageData, LogicalPoint, LogicalRect, Observation, ObservationId,
        OutputId, OutputTransform, PixelSize, SourceSelector,
    };

    use super::{CapturedObservation, observed_element, process_directory_absent};

    fn node(handle: &str) -> AccessibilityNode {
        AccessibilityNode {
            handle: ElementHandle(handle.into()),
            application: "test".into(),
            window_id: None,
            role: "push button".into(),
            name: None,
            value: None,
            text: None,
            states: Vec::new(),
            bounds: None,
            actions: Vec::new(),
            children: Vec::new(),
        }
    }

    fn captured(nodes: Vec<AccessibilityNode>, complete: bool) -> CapturedObservation {
        let bounds = LogicalRect {
            origin: LogicalPoint { x: 0.0, y: 0.0 },
            width: 10.0,
            height: 10.0,
        };
        let pixels = PixelSize {
            width: 10,
            height: 10,
        };
        CapturedObservation {
            observation: Observation {
                id: ObservationId::new(1),
                captured_at_unix_ms: 0,
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                stable_output_id: OutputId("DP-1".into()),
                capture: CaptureProvenance {
                    route: CaptureRoute::FullOutput,
                    output_local_bounds: None,
                },
                image: ImageData {
                    mime_type: "image/png".into(),
                    data_base64: String::new(),
                },
                coordinates: CoordinateMapping {
                    source_pixels: pixels,
                    model_image_pixels: pixels,
                    logical_bounds: bounds,
                    scale: 1.0,
                    transform: OutputTransform::Normal,
                },
                focus: FocusState {
                    window_id: None,
                    element: None,
                },
                accessibility: nodes,
                accessibility_available: true,
                accessibility_limitation: None,
                continuation: None,
            },
            frame_fingerprint: 0,
            application_frame_fingerprint: 0,
            accessibility_complete: complete,
        }
    }

    #[test]
    fn an_element_missing_from_a_cut_short_tree_is_unmeasurable() {
        let missing = ElementHandle("missing".into());
        let error = observed_element(&captured(vec![node("present")], false), &missing)
            .expect_err("absence from a cut-short tree proves nothing");
        assert_eq!(error.code, ErrorCode::BackendFailure);
        assert!(error.message.contains("missing"), "{}", error.message);

        assert!(
            observed_element(&captured(vec![node("present")], true), &missing)
                .unwrap()
                .is_none()
        );
        assert!(
            observed_element(
                &captured(vec![node("present")], false),
                &ElementHandle("present".into())
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn a_process_filesystem_that_cannot_be_read_is_not_an_exit() {
        let error = process_directory_absent(Path::new("/nonexistent-waldo-proc"), 1)
            .expect_err("a missing process filesystem measures nothing");
        assert_eq!(error.code, ErrorCode::BackendFailure);

        assert!(!process_directory_absent(Path::new("/proc"), std::process::id()).unwrap());
        assert!(process_directory_absent(Path::new("/proc"), u32::MAX).unwrap());
    }
}
