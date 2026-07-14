use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::hash::{DefaultHasher, Hash, Hasher};
use waldo_controller::ObservationGrounding;
use waldo_protocol::{
    CoordinateMapping, ErrorCode, FocusState, ImageData, LogicalRect, Observation, OutputState,
    ProtocolError, RequestId, ServerMessage, SourceSelector, WindowId,
};
use waldo_screencopy::{Capture, ScreenCapturer};

use crate::server::ServiceState;

pub(crate) async fn observe(
    request_id: RequestId,
    source: SourceSelector,
    include_accessibility: bool,
    state: &ServiceState,
    connection_id: waldo_protocol::ConnectionId,
) -> ServerMessage {
    match capture_observation(source, include_accessibility, state, connection_id).await {
        Ok(observation) => ServerMessage::Observed {
            request_id,
            observation,
        },
        Err(error) => ServerMessage::Error {
            request_id: Some(request_id),
            error,
        },
    }
}

pub(crate) async fn capture_observation(
    source: SourceSelector,
    include_accessibility: bool,
    state: &ServiceState,
    connection_id: waldo_protocol::ConnectionId,
) -> Result<Observation, ProtocolError> {
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
        capture,
    } = match captured {
        Ok(Ok(captured)) => captured,
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(backend_error(error.to_string())),
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
        },
    );
    let pixel_size = waldo_protocol::PixelSize {
        width: capture.width,
        height: capture.height,
    };

    let (accessibility, focused_element, accessibility_available) = if include_accessibility {
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
                    Ok(Ok(snapshot)) => (snapshot.nodes, snapshot.focused, true),
                    _ => (Vec::new(), None, false),
                }
            }
            _ => (Vec::new(), None, false),
        }
    } else {
        (Vec::new(), None, false)
    };

    Ok(Observation {
        id: observation_id,
        captured_at_unix_ms: capture.captured_at_unix_ms,
        source,
        stable_output_id: output.id,
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
        cursor: None,
        focus: FocusState {
            window_id: focused_window,
            element: focused_element,
        },
        accessibility,
        accessibility_available,
    })
}

pub(crate) struct CapturedFrame {
    pub(crate) focused_window: Option<WindowId>,
    pub(crate) focused_window_state: Option<waldo_protocol::WindowState>,
    pub(crate) output: OutputState,
    pub(crate) logical_bounds: LogicalRect,
    pub(crate) capture: Capture,
}

pub(crate) fn capture_frame(
    source: &SourceSelector,
    inspector: &dyn waldo_niri::SessionInspector,
    capturer: &dyn ScreenCapturer,
    include_windows: bool,
) -> Result<CapturedFrame, ProtocolError> {
    let snapshot = inspector
        .inspect(include_windows || matches!(source, SourceSelector::Window { .. }))
        .map_err(|error| backend_error(error.to_string()))?;
    let (output_id, requested_region) = resolve_capture_source(source, &snapshot)?;
    let focused_window = snapshot.focused_window;
    let focused_window_state = focused_window
        .as_ref()
        .and_then(|id| snapshot.windows.iter().find(|window| &window.id == id))
        .cloned();
    let output = snapshot
        .outputs
        .into_iter()
        .find(|output| output.id == output_id)
        .ok_or_else(|| target_error(format!("Niri output is unavailable: {}", output_id.0)))?;
    let logical_bounds = requested_region.unwrap_or(output.logical_bounds);
    let capture = match requested_region {
        Some(region) => {
            let (x, y, width, height) = region_coordinates(region, output.logical_bounds)?;
            capturer.capture_region(&output.name, x, y, width, height)
        }
        None => capturer.capture_output(&output.name),
    }
    .map_err(|error| backend_error(error.to_string()))?;

    Ok(CapturedFrame {
        focused_window,
        focused_window_state,
        output,
        logical_bounds,
        capture,
    })
}

fn resolve_capture_source(
    source: &SourceSelector,
    snapshot: &waldo_niri::NiriSnapshot,
) -> Result<
    (
        waldo_protocol::OutputId,
        Option<waldo_protocol::LogicalRect>,
    ),
    ProtocolError,
> {
    match source {
        SourceSelector::Output { output_id } => Ok((output_id.clone(), None)),
        SourceSelector::Region { output_id, region } => Ok((output_id.clone(), Some(*region))),
        SourceSelector::Window { window_id } => {
            let window = snapshot
                .windows
                .iter()
                .find(|window| &window.id == window_id)
                .ok_or_else(|| {
                    target_error(format!("Niri window is unavailable: {}", window_id.0))
                })?;
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
            let bounds = window.logical_bounds.ok_or_else(|| ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!(
                    "Niri does not expose screen bounds for window {}; observe its output instead",
                    window_id.0
                ),
                retryable: false,
            })?;
            Ok((output_id, Some(bounds)))
        }
    }
}

pub(crate) fn frame_fingerprint(png: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    png.hash(&mut hasher);
    hasher.finish()
}

fn region_coordinates(
    region: waldo_protocol::LogicalRect,
    output: waldo_protocol::LogicalRect,
) -> Result<(i32, i32, i32, i32), ProtocolError> {
    let local_x = region.origin.x - output.origin.x;
    let local_y = region.origin.y - output.origin.y;
    let values = [local_x, local_y, region.width, region.height];
    if values
        .iter()
        .any(|value| !value.is_finite() || value.fract() != 0.0)
    {
        return Err(backend_error(
            "capture region must use finite whole logical pixels".into(),
        ));
    }
    if local_x < 0.0
        || local_y < 0.0
        || region.width <= 0.0
        || region.height <= 0.0
        || local_x + region.width > output.width
        || local_y + region.height > output.height
    {
        return Err(backend_error(
            "capture region lies outside the selected output".into(),
        ));
    }
    let convert = |value: f64| {
        i32::try_from(value as i64).map_err(|_| {
            backend_error("capture region coordinate exceeds the protocol range".into())
        })
    };
    Ok((
        convert(local_x)?,
        convert(local_y)?,
        convert(region.width)?,
        convert(region.height)?,
    ))
}

fn backend_error(message: String) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message,
        retryable: true,
    }
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
        LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform, SourceSelector,
        WindowId, WindowState, WorkspaceId, WorkspaceState,
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

        assert!(region_coordinates(fractional, OUTPUT).is_err());
        assert!(region_coordinates(crossing, OUTPUT).is_err());
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
            focused: true,
            workspace_id: Some(WorkspaceId::new(1)),
            output_id: Some(OutputId("DP-2".into())),
        }];

        let (_, region) = resolve_capture_source(
            &SourceSelector::Window {
                window_id: WindowId("10".into()),
            },
            &snapshot,
        )
        .unwrap();
        assert_eq!(region, snapshot.windows[0].logical_bounds);

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
    }
}
