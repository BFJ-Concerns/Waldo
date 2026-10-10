use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::hash::{DefaultHasher, Hash, Hasher};
use waldo_controller::ObservationGrounding;
use waldo_protocol::{
    CaptureProvenance, CaptureRoute, CoordinateMapping, ErrorCode, FocusState, ImageData,
    LogicalPoint, LogicalRect, Observation, ObservationContinuation, ObservationId, OutputState,
    ProtocolError, RequestId, ServerMessage, SourceSelector, WindowId,
};
use waldo_screencopy::{Capture, ScreenCapturer};

use crate::server::ServiceState;

pub(crate) async fn observe(
    request_id: RequestId,
    source: SourceSelector,
    continue_from: Option<ObservationId>,
    include_accessibility: bool,
    state: &ServiceState,
    connection_id: waldo_protocol::ConnectionId,
) -> ServerMessage {
    let previous = if let Some(previous_id) = continue_from {
        let controller = state.controller.lock().await;
        let Some(previous) = controller.observation(connection_id, previous_id).cloned() else {
            return ServerMessage::Error {
                request_id: Some(request_id),
                error: ProtocolError {
                    code: ErrorCode::StaleObservation,
                    message: "continue_from references an observation which is not retained for this connection".into(),
                    retryable: true,
                },
            };
        };
        if previous.source != source {
            return ServerMessage::Error {
                request_id: Some(request_id),
                error: ProtocolError::invalid_request(
                    "continue_from must reference the same observation source",
                ),
            };
        }
        Some((previous_id, previous))
    } else {
        None
    };

    match capture_observation(source, include_accessibility, state, connection_id).await {
        Ok(CapturedObservation {
            mut observation, ..
        }) => {
            if let Some((previous_id, previous)) = previous {
                let Some(current) = state
                    .controller
                    .lock()
                    .await
                    .observation(connection_id, observation.id)
                    .cloned()
                else {
                    return ServerMessage::Error {
                        request_id: Some(request_id),
                        error: ProtocolError {
                            code: ErrorCode::StaleObservation,
                            message: "continued observation was evicted before its continuity could be reported".into(),
                            retryable: true,
                        },
                    };
                };
                observation.continuation = Some(continuation(previous_id, &previous, &current));
            }
            ServerMessage::Observed {
                request_id,
                observation,
            }
        }
        Err(error) => ServerMessage::Error {
            request_id: Some(request_id),
            error,
        },
    }
}

/// A recorded observation with the frame evidence it was recorded with, so a
/// caller comparing frames never depends on the observation still being
/// retained after other clients have observed.
pub(crate) struct CapturedObservation {
    pub(crate) observation: Observation,
    pub(crate) frame_fingerprint: u64,
    pub(crate) application_frame_fingerprint: u64,
    /// Whether the accessibility snapshot covered the whole window tree;
    /// false when it was cut short or no snapshot was taken.
    pub(crate) accessibility_complete: bool,
}

pub(crate) async fn capture_observation(
    source: SourceSelector,
    include_accessibility: bool,
    state: &ServiceState,
    connection_id: waldo_protocol::ConnectionId,
) -> Result<CapturedObservation, ProtocolError> {
    let capture_source = source.clone();
    let inspector = std::sync::Arc::clone(&state.inspector);
    let capturer = std::sync::Arc::clone(&state.capturer);
    let captured = tokio::task::spawn_blocking(move || {
        capture_frame(
            &capture_source,
            inspector.as_ref(),
            capturer.as_ref(),
            include_accessibility,
        )
    })
    .await;
    let CapturedFrame {
        focused_window,
        focused_window_state,
        output,
        logical_bounds,
        provenance,
        capture,
        application_frame_fingerprint,
    } = match captured {
        Ok(Ok(captured)) => captured,
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(ProtocolError::backend_failure(error.to_string(), true)),
    };
    let frame_fingerprint = frame_fingerprint(&capture.png);
    let observation_id = state.controller.lock().await.record_observation(
        connection_id,
        ObservationGrounding {
            source: source.clone(),
            output_id: output.id.clone(),
            output_logical_bounds: output.logical_bounds,
            observed_logical_bounds: logical_bounds,
            scale: output.scale,
            transform: output.transform,
            focused_window: focused_window.clone(),
            include_accessibility,
            cursor_position: None,
            frame_fingerprint,
            application_frame_fingerprint,
        },
    );
    let pixel_size = waldo_protocol::PixelSize {
        width: capture.width,
        height: capture.height,
    };

    let (
        accessibility,
        focused_element,
        accessibility_available,
        accessibility_complete,
        accessibility_limitation,
    ) = if include_accessibility {
        match (&state.accessibility, focused_window_state) {
            (Some(backend), Some(window)) if window.pid.is_some() => {
                let backend = std::sync::Arc::clone(backend);
                let pid = window.pid.expect("guarded by is_some");
                let window_id = window.id;
                let application_id = window.application_id;
                let window_title = window.title;
                match tokio::task::spawn_blocking(move || {
                    backend.snapshot(pid, window_id, application_id, window_title, logical_bounds)
                })
                .await
                {
                    Ok(Ok(snapshot)) => {
                        let limitation =
                            (!snapshot.complete).then(|| INCOMPLETE_TREE_LIMITATION.to_owned());
                        (
                            snapshot.nodes,
                            snapshot.focused,
                            true,
                            snapshot.complete,
                            limitation,
                        )
                    }
                    Ok(Err(error)) if error.code == ErrorCode::AmbiguousTarget => {
                        return Err(error);
                    }
                    Ok(Err(error)) => (
                        Vec::new(),
                        None,
                        false,
                        false,
                        Some(format!("accessibility snapshot failed: {}", error.message)),
                    ),
                    Err(error) => (
                        Vec::new(),
                        None,
                        false,
                        false,
                        Some(format!("accessibility snapshot task failed: {error}")),
                    ),
                }
            }
            _ => (Vec::new(), None, false, false, None),
        }
    } else {
        (Vec::new(), None, false, false, None)
    };

    let observation = Observation {
        id: observation_id,
        captured_at_unix_ms: capture.captured_at_unix_ms,
        source,
        stable_output_id: output.id,
        capture: provenance,
        image: ImageData {
            mime_type: "image/png".into(),
            data_base64: BASE64.encode(capture.png),
        },
        coordinates: CoordinateMapping {
            source_pixels: pixel_size,
            model_image_pixels: pixel_size,
            logical_bounds,
            scale: output.scale,
            transform: output.transform,
        },
        focus: FocusState {
            window_id: focused_window,
            element: focused_element,
        },
        accessibility,
        accessibility_available,
        accessibility_limitation,
        continuation: None,
    };
    Ok(CapturedObservation {
        observation,
        frame_fingerprint,
        application_frame_fingerprint,
        accessibility_complete,
    })
}

/// Tells the agent that elements missing from a cut-short tree may exist.
const INCOMPLETE_TREE_LIMITATION: &str = "the accessibility tree was too large to read whole; elements and focus beyond the nodes listed were not inspected";

pub(crate) struct CapturedFrame {
    pub(crate) focused_window: Option<WindowId>,
    pub(crate) focused_window_state: Option<waldo_protocol::WindowState>,
    pub(crate) output: OutputState,
    pub(crate) logical_bounds: LogicalRect,
    pub(crate) provenance: CaptureProvenance,
    pub(crate) capture: Capture,
    pub(crate) application_frame_fingerprint: u64,
}

pub(crate) fn capture_frame(
    source: &SourceSelector,
    inspector: &dyn waldo_niri::SessionInspector,
    capturer: &dyn ScreenCapturer,
    include_windows: bool,
) -> Result<CapturedFrame, ProtocolError> {
    let snapshot = inspector
        .inspect(include_windows || matches!(source, SourceSelector::Window { .. }))
        .map_err(|error| ProtocolError::backend_failure(error.to_string(), true))?;
    let resolved = resolve_capture_source(source, &snapshot)?;
    let focused_window = snapshot.focused_window;
    let focused_window_state = focused_window
        .as_ref()
        .and_then(|id| snapshot.windows.iter().find(|window| &window.id == id))
        .cloned();
    let output = snapshot
        .outputs
        .into_iter()
        .find(|output| output.id == resolved.output_id)
        .ok_or_else(|| {
            target_error(format!(
                "Niri output is unavailable: {}",
                resolved.output_id.0
            ))
        })?;
    let logical_bounds = resolved.region.unwrap_or(output.logical_bounds);
    let output_local_bounds = resolved.region.map(|region| LogicalRect {
        origin: LogicalPoint {
            x: region.origin.x - output.logical_bounds.origin.x,
            y: region.origin.y - output.logical_bounds.origin.y,
        },
        width: region.width,
        height: region.height,
    });
    let capture = match resolved.region {
        Some(region) => {
            let (x, y, width, height) = region_coordinates(region, output.logical_bounds)?;
            capturer.capture_region(&output.name, x, y, width, height)
        }
        None => capturer.capture_output(&output.name),
    }
    .map_err(|error| ProtocolError::backend_failure(error.to_string(), true))?;
    let application_capture = match resolved.region {
        Some(region) => {
            let (x, y, width, height) = region_coordinates(region, output.logical_bounds)?;
            capturer.capture_application_region(&output.name, x, y, width, height)
        }
        None => capturer.capture_application_output(&output.name),
    }
    .map_err(|error| ProtocolError::backend_failure(error.to_string(), true))?;

    Ok(CapturedFrame {
        focused_window,
        focused_window_state,
        output,
        logical_bounds,
        provenance: CaptureProvenance {
            route: resolved.route,
            output_local_bounds,
        },
        capture,
        application_frame_fingerprint: frame_fingerprint(&application_capture.png),
    })
}

#[derive(Debug)]
struct ResolvedCaptureSource {
    output_id: waldo_protocol::OutputId,
    region: Option<waldo_protocol::LogicalRect>,
    route: CaptureRoute,
}

fn resolve_capture_source(
    source: &SourceSelector,
    snapshot: &waldo_niri::NiriSnapshot,
) -> Result<ResolvedCaptureSource, ProtocolError> {
    match source {
        SourceSelector::Output { output_id } => Ok(ResolvedCaptureSource {
            output_id: output_id.clone(),
            region: None,
            route: CaptureRoute::FullOutput,
        }),
        SourceSelector::Region { output_id, region } => Ok(ResolvedCaptureSource {
            output_id: output_id.clone(),
            region: Some(*region),
            route: CaptureRoute::OutputRegion,
        }),
        SourceSelector::Window { window_id } => {
            let window = snapshot
                .windows
                .iter()
                .find(|window| &window.id == window_id)
                .ok_or_else(|| {
                    target_error(format!("Niri window is unavailable: {}", window_id.0))
                })?;
            if !window.capture.available {
                return Err(ProtocolError::unsupported_capability(
                    window.capture.limitation.clone().unwrap_or_else(|| {
                        format!("Niri window {} cannot be captured directly", window_id.0)
                    }),
                ));
            }
            if !window.focused {
                return Err(ProtocolError {
                    code: ErrorCode::FocusChanged,
                    message: format!(
                        "Niri window {} is not foreground; focus it before direct window observation",
                        window_id.0
                    ),
                    retryable: true,
                });
            }
            let output_id = window.output_id.clone().ok_or_else(|| {
                target_error(format!(
                    "Niri has not assigned window {} to an output",
                    window_id.0
                ))
            })?;
            let visible = window.workspace_id.is_some_and(|workspace_id| {
                snapshot.workspaces.iter().any(|workspace| {
                    workspace.id == workspace_id
                        && workspace.active
                        && workspace.output_id.as_ref() == Some(&output_id)
                })
            });
            if !visible {
                return Err(target_error(format!(
                    "Niri window {} is not visible on an active workspace",
                    window_id.0
                )));
            }
            let bounds = window.logical_bounds.ok_or_else(|| {
                ProtocolError::unsupported_capability(format!(
                    "Niri does not expose screen bounds for window {}; observe its output instead",
                    window_id.0
                ))
            })?;
            Ok(ResolvedCaptureSource {
                output_id,
                region: Some(bounds),
                route: CaptureRoute::VisibleWindowCrop,
            })
        }
    }
}

fn continuation(
    from: ObservationId,
    previous: &ObservationGrounding,
    current: &ObservationGrounding,
) -> ObservationContinuation {
    ObservationContinuation {
        from,
        frame_changed: previous.frame_fingerprint != current.frame_fingerprint,
        geometry_changed: previous.output_id != current.output_id
            || previous.output_logical_bounds != current.output_logical_bounds
            || previous.observed_logical_bounds != current.observed_logical_bounds
            || previous.scale != current.scale
            || previous.transform != current.transform,
        focus_changed: previous.focused_window != current.focused_window,
    }
}

pub(crate) fn frame_fingerprint(png: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    png.hash(&mut hasher);
    hasher.finish()
}

/// Converts a client-supplied desktop region into the output-local whole
/// pixels screencopy takes. A region the client could not have meant is an
/// invalid request; output geometry the compositor reported in fractional
/// pixels is a backend fault the client cannot repair by rewording.
fn region_coordinates(
    region: waldo_protocol::LogicalRect,
    output: waldo_protocol::LogicalRect,
) -> Result<(i32, i32, i32, i32), ProtocolError> {
    let whole = |value: f64| value.is_finite() && value.fract() == 0.0;
    if ![
        region.origin.x,
        region.origin.y,
        region.width,
        region.height,
    ]
    .iter()
    .all(|value| whole(*value))
    {
        return Err(ProtocolError::invalid_request(
            "capture region must use finite whole logical pixels",
        ));
    }
    if region.width <= 0.0 || region.height <= 0.0 {
        return Err(ProtocolError::invalid_request(
            "capture region must have a positive width and height",
        ));
    }
    if ![
        output.origin.x,
        output.origin.y,
        output.width,
        output.height,
    ]
    .iter()
    .all(|value| whole(*value))
    {
        return Err(ProtocolError::backend_failure(
            "the selected output's logical geometry is not whole pixels, so a region cannot be captured",
            false,
        ));
    }
    let local_x = region.origin.x - output.origin.x;
    let local_y = region.origin.y - output.origin.y;
    if local_x < 0.0
        || local_y < 0.0
        || local_x + region.width > output.width
        || local_y + region.height > output.height
    {
        return Err(ProtocolError::invalid_request(
            "capture region lies outside the selected output",
        ));
    }
    let convert = |value: f64| {
        i32::try_from(value as i64).map_err(|_| {
            ProtocolError::invalid_request("capture region coordinate exceeds the protocol range")
        })
    };
    Ok((
        convert(local_x)?,
        convert(local_y)?,
        convert(region.width)?,
        convert(region.height)?,
    ))
}

fn target_error(message: String) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::TargetLost,
        message,
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use waldo_niri::NiriSnapshot;
    use waldo_protocol::{
        ErrorCode, LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform,
        SourceSelector, WindowId, WindowState, WorkspaceId, WorkspaceState,
    };

    use super::{region_coordinates, resolve_capture_source};

    const OUTPUT: LogicalRect = LogicalRect {
        origin: LogicalPoint { x: 2560.0, y: 0.0 },
        width: 2560.0,
        height: 1440.0,
    };

    #[test]
    fn desktop_regions_are_converted_to_output_local_coordinates() {
        let region = LogicalRect {
            origin: LogicalPoint { x: 2660.0, y: 50.0 },
            width: 640.0,
            height: 480.0,
        };

        assert_eq!(
            region_coordinates(region, OUTPUT).unwrap(),
            (100, 50, 640, 480)
        );
    }

    #[test]
    fn fractional_or_out_of_bounds_regions_fail_instead_of_shifting() {
        let fractional = LogicalRect {
            origin: LogicalPoint { x: 2560.5, y: 0.0 },
            width: 100.0,
            height: 100.0,
        };
        let crossing = LogicalRect {
            origin: LogicalPoint { x: 5000.0, y: 0.0 },
            width: 200.0,
            height: 100.0,
        };

        for region in [fractional, crossing] {
            let error = region_coordinates(region, OUTPUT).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{}", error.message);
            assert!(!error.retryable, "{}", error.message);
        }
    }

    #[test]
    fn fractional_output_geometry_is_the_backend_fault_not_the_request() {
        let region = LogicalRect {
            origin: LogicalPoint { x: 2660.0, y: 50.0 },
            width: 640.0,
            height: 480.0,
        };
        let fractional_output = LogicalRect {
            origin: LogicalPoint { x: 2560.5, y: 0.0 },
            ..OUTPUT
        };

        let error = region_coordinates(region, fractional_output).unwrap_err();

        assert_eq!(error.code, ErrorCode::BackendFailure, "{}", error.message);
        assert!(!error.retryable, "{}", error.message);
    }

    #[test]
    fn visible_window_capture_requires_exact_screen_bounds() {
        let mut snapshot = NiriSnapshot::unavailable("test");
        snapshot.outputs = vec![OutputState {
            id: OutputId("DP-2".into()),
            name: "DP-2".into(),
            logical_bounds: OUTPUT,
            pixel_size: None,
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused: true,
        }];
        snapshot.workspaces = vec![WorkspaceState {
            id: WorkspaceId::new(1),
            index: 1,
            name: None,
            output_id: Some(OutputId("DP-2".into())),
            focused: true,
            active: true,
        }];
        snapshot.windows = vec![WindowState {
            id: WindowId("10".into()),
            title: None,
            application_id: None,
            pid: None,
            logical_bounds: Some(LogicalRect {
                origin: LogicalPoint {
                    x: 3000.0,
                    y: 200.0,
                },
                width: 400.0,
                height: 300.0,
            }),
            is_floating: true,
            capture: waldo_protocol::WindowCaptureCapability {
                available: true,
                limitation: None,
            },
            focused: true,
            workspace_id: Some(WorkspaceId::new(1)),
            output_id: Some(OutputId("DP-2".into())),
        }];

        let resolved = resolve_capture_source(
            &SourceSelector::Window {
                window_id: WindowId("10".into()),
            },
            &snapshot,
        )
        .unwrap();
        assert_eq!(resolved.region, snapshot.windows[0].logical_bounds);
        assert_eq!(
            resolved.route,
            waldo_protocol::CaptureRoute::VisibleWindowCrop
        );

        snapshot.windows[0].focused = false;
        assert!(
            resolve_capture_source(
                &SourceSelector::Window {
                    window_id: WindowId("10".into()),
                },
                &snapshot,
            )
            .is_err()
        );

        snapshot.windows[0].focused = true;
        snapshot.windows[0].logical_bounds = None;
        assert!(
            resolve_capture_source(
                &SourceSelector::Window {
                    window_id: WindowId("10".into()),
                },
                &snapshot,
            )
            .is_err()
        );

        snapshot.windows[0].logical_bounds = Some(OUTPUT);
        snapshot.windows[0].is_floating = false;
        snapshot.windows[0].capture = waldo_protocol::WindowCaptureCapability {
            available: false,
            limitation: Some("tiled-window capture is unavailable".into()),
        };
        let error = resolve_capture_source(
            &SourceSelector::Window {
                window_id: WindowId("10".into()),
            },
            &snapshot,
        )
        .unwrap_err();
        assert_eq!(error.code, waldo_protocol::ErrorCode::UnsupportedCapability);
        assert!(error.message.contains("tiled-window"));
    }
}
