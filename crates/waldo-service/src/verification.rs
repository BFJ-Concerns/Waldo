use std::{collections::BTreeMap, path::Path, sync::Arc};

use waldo_niri::NiriSnapshot;
use waldo_protocol::{
    ConnectionId, ErrorCode, ObservationId, Predicate, ProtocolError, SourceSelector, WindowId,
};

use crate::{error::backend_error, observe::CapturedObservation, server::ServiceState};

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
                        return Err(ProtocolError {
                            code: ErrorCode::InvalidRequest,
                            message:
                                "frame-change verification must compare the source the batch's resulting observation captures"
                                    .into(),
                            retryable: false,
                        });
                    }
                    prior_frames.insert(*from, grounding.application_frame_fingerprint);
                }
                Predicate::ElementExists { .. } | Predicate::ElementState { .. } => {
                    if !include_accessibility {
                        return Err(ProtocolError {
                            code: ErrorCode::InvalidRequest,
                            message: "element verification needs accessibility evidence; act on an observation made with include_accessibility".into(),
                            retryable: false,
                        });
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
            return Err(ProtocolError {
                code: ErrorCode::BackendFailure,
                message: format!(
                    "element verification could not be measured: {}",
                    observation
                        .accessibility_limitation
                        .as_deref()
                        .unwrap_or("the resulting observation has no accessibility evidence")
                ),
                retryable: true,
            });
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
                Predicate::ElementExists { element } => Ok(observation
                    .accessibility
                    .iter()
                    .any(|node| &node.handle == element)),
                Predicate::ElementState {
                    element,
                    state,
                    present,
                } => Ok(observation
                    .accessibility
                    .iter()
                    .find(|node| &node.handle == element)
                    .is_some_and(|node| {
                        node.states.iter().any(|actual| actual == state) == *present
                    })),
            })
            .collect()
    }
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
    Ok(!Path::new(&format!("/proc/{pid}")).exists())
}

fn validate_pid(pid: u32) -> Result<(), ProtocolError> {
    if pid == 0 {
        return Err(ProtocolError {
            code: ErrorCode::InvalidRequest,
            message: "a process predicate requires a non-zero PID".into(),
            retryable: false,
        });
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
    backend_error(format!(
        "could not inspect post-action verification state: {message}"
    ))
}
