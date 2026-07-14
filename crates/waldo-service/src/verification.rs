use std::{collections::BTreeMap, path::Path, sync::Arc};

use waldo_protocol::{
    ConnectionId, ErrorCode, Observation, ObservationId, Predicate, ProtocolError, SourceSelector,
};

use crate::server::ServiceState;

/// Predicates validated before mutation with any retained frame evidence attached.
pub(crate) struct VerificationPlan {
    predicates: Vec<Predicate>,
    prior_frames: BTreeMap<ObservationId, u64>,
}

impl VerificationPlan {
    pub(crate) async fn prepare(
        predicates: Vec<Predicate>,
        source: &SourceSelector,
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
                    if &grounding.source != source {
                        return Err(ProtocolError {
                            code: ErrorCode::InvalidRequest,
                            message:
                                "frame-change verification must compare the same capture source"
                                    .into(),
                            retryable: false,
                        });
                    }
                    prior_frames.insert(*from, grounding.frame_fingerprint);
                }
                Predicate::ElementExists { .. } | Predicate::ElementState { .. } => {}
                Predicate::WindowExists { .. }
                | Predicate::FocusedWindow { .. }
                | Predicate::ProcessExited { .. } => {}
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
        observation: &Observation,
        state: &ServiceState,
        connection_id: ConnectionId,
    ) -> Result<Vec<bool>, ProtocolError> {
        if self.predicates.is_empty() {
            return Ok(Vec::new());
        }
        let current_frame = state
            .controller
            .lock()
            .await
            .observation(connection_id, observation.id)
            .map(|grounding| grounding.frame_fingerprint)
            .ok_or_else(|| ProtocolError {
                code: ErrorCode::Internal,
                message: "settled observation was not retained for verification".into(),
                retryable: true,
            })?;
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

        Ok(self
            .predicates
            .iter()
            .map(|predicate| match predicate {
                Predicate::FrameChanged { from } => self
                    .prior_frames
                    .get(from)
                    .is_some_and(|prior| *prior != current_frame),
                Predicate::WindowExists { window_id } => snapshot
                    .as_ref()
                    .expect("window predicates requested an inspection")
                    .windows
                    .iter()
                    .any(|window| &window.id == window_id),
                Predicate::FocusedWindow { window_id } => {
                    snapshot
                        .as_ref()
                        .expect("window predicates requested an inspection")
                        .focused_window
                        .as_ref()
                        == Some(window_id)
                }
                Predicate::ProcessExited { pid } => !Path::new(&format!("/proc/{pid}")).exists(),
                Predicate::ElementExists { element } => observation
                    .accessibility
                    .iter()
                    .any(|node| &node.handle == element),
                Predicate::ElementState {
                    element,
                    state,
                    present,
                } => observation
                    .accessibility
                    .iter()
                    .find(|node| &node.handle == element)
                    .is_some_and(|node| {
                        node.states.iter().any(|actual| actual == state) == *present
                    }),
            })
            .collect())
    }
}

pub(crate) fn failed_verification() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::VerificationFailed,
        message: "one or more post-action verification predicates were false".into(),
        retryable: true,
    }
}

fn verification_backend(message: String) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message: format!("could not inspect post-action verification state: {message}"),
        retryable: true,
    }
}
