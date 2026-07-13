use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use waldo_protocol::{
    CoordinateMapping, ErrorCode, FocusState, ImageData, Observation, ProtocolError, RequestId,
    ServerMessage, SourceSelector,
};

use crate::server::ServiceState;

pub(crate) async fn observe(
    request_id: RequestId,
    source: SourceSelector,
    state: &ServiceState,
) -> ServerMessage {
    let (output_id, requested_region) = match &source {
        SourceSelector::Output { output_id } => (output_id.clone(), None),
        SourceSelector::Region { output_id, region } => (output_id.clone(), Some(*region)),
        SourceSelector::Window { .. } => {
            return unsupported(request_id, "foreground-window crop");
        }
    };
    let inspector = std::sync::Arc::clone(&state.inspector);
    let capturer = std::sync::Arc::clone(&state.capturer);
    let captured = tokio::task::spawn_blocking(move || {
        let snapshot = inspector
            .inspect(false)
            .map_err(|error| error.to_string())?;
        let focused_window = snapshot.focused_window;
        let output = snapshot
            .outputs
            .into_iter()
            .find(|output| output.id == output_id)
            .ok_or_else(|| format!("Niri output is unavailable: {}", output_id.0))?;
        let logical_bounds = requested_region.unwrap_or(output.logical_bounds);
        let capture = match requested_region {
            Some(region) => {
                let (x, y, width, height) = region_coordinates(region, output.logical_bounds)?;
                capturer.capture_region(&output.name, x, y, width, height)
            }
            None => capturer.capture_output(&output.name),
        }
        .map_err(|error| error.to_string())?;
        Ok::<_, String>((focused_window, output, logical_bounds, capture))
    })
    .await;
    let (focused_window, output, logical_bounds, capture) = match captured {
        Ok(Ok(captured)) => captured,
        Ok(Err(error)) => return backend_error(request_id, error),
        Err(error) => return backend_error(request_id, error.to_string()),
    };
    let observation_id = state.controller.lock().await.next_observation_id();
    let pixel_size = waldo_protocol::PixelSize {
        width: capture.width,
        height: capture.height,
    };

    ServerMessage::Observed {
        request_id,
        observation: Observation {
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
                element: None,
            },
            accessibility: Vec::new(),
            accessibility_available: false,
        },
    }
}

fn region_coordinates(
    region: waldo_protocol::LogicalRect,
    output: waldo_protocol::LogicalRect,
) -> Result<(i32, i32, i32, i32), String> {
    let local_x = region.origin.x - output.origin.x;
    let local_y = region.origin.y - output.origin.y;
    let values = [local_x, local_y, region.width, region.height];
    if values
        .iter()
        .any(|value| !value.is_finite() || value.fract() != 0.0)
    {
        return Err("capture region must use finite whole logical pixels".into());
    }
    if local_x < 0.0
        || local_y < 0.0
        || region.width <= 0.0
        || region.height <= 0.0
        || local_x + region.width > output.width
        || local_y + region.height > output.height
    {
        return Err("capture region lies outside the selected output".into());
    }
    let convert = |value: f64| {
        i32::try_from(value as i64)
            .map_err(|_| "capture region coordinate exceeds the protocol range".to_owned())
    };
    Ok((
        convert(local_x)?,
        convert(local_y)?,
        convert(region.width)?,
        convert(region.height)?,
    ))
}

fn backend_error(request_id: RequestId, message: String) -> ServerMessage {
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code: ErrorCode::BackendFailure,
            message,
            retryable: true,
        },
    }
}

fn unsupported(request_id: RequestId, capability: &str) -> ServerMessage {
    ServerMessage::Error {
        request_id: Some(request_id),
        error: ProtocolError {
            code: ErrorCode::UnsupportedCapability,
            message: format!("{capability} is not initialised"),
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use waldo_protocol::{LogicalPoint, LogicalRect};

    use super::region_coordinates;

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
}
