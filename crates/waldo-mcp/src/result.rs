use std::{io::Cursor, num::NonZeroU32};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{GenericImageView, ImageFormat, imageops::FilterType};
use serde_json::{Map, Value, json};
use waldo_protocol::{ErrorCode, Observation, PixelSize, ProtocolError, ServerMessage, StopReason};

use crate::tools::ToolResult;
use waldo_socket::TransportError;

/// Renders a controller response as an MCP tool result. `isError` is set for
/// a protocol error, a batch that did not complete, a Stop whose clean-up
/// left desktop state unrestored, and an observation image that could not
/// be presented; an inconclusive application outcome on a completed batch
/// is not an error. The structured content is the controller's whole
/// response either way, so a Stop's released session and clean-up
/// limitation stay readable beside the error flag.
pub(crate) fn tool_response(mut response: ServerMessage, max_edge: NonZeroU32) -> Value {
    if let ServerMessage::Error { error, .. } = &response {
        return tool_failure(error.clone());
    }

    let batch_failed = matches!(
        &response,
        ServerMessage::Acted { result, .. } if result.stop_reason != StopReason::Completed
    );
    let cleanup_failed = matches!(
        &response,
        ServerMessage::Stopped { result, .. } if result.cleanup_limitation.is_some()
    );
    let presentation = present_response_images(&mut response, max_edge);
    let mut response = serde_json::to_value(response).expect("controller responses serialise");
    if let Err(error) = &presentation {
        // The receipts stay: the actions they report ran whether or not the
        // resulting image can be shown, and repeating them is not safe.
        withhold_image(&mut response);
        response["presentation_error"] = serde_json::to_value(ProtocolError {
            code: ErrorCode::Internal,
            message: format!("image presentation failed: {error}"),
            retryable: false,
        })
        .expect("protocol errors serialise");
    }
    let (structured, image) = separate_image(response);
    let mut content = vec![json!({
        "type": "text",
        "text": serde_json::to_string(&structured).expect("structured response serialises")
    })];
    if let Some((mime_type, data)) = image {
        content.push(json!({ "type": "image", "mimeType": mime_type, "data": data }));
    }
    serde_json::to_value(ToolResult {
        content,
        structured_content: Some(structured),
        is_error: batch_failed || cleanup_failed || presentation.is_err(),
    })
    .expect("tool result serialises")
}

/// Removes an observation image that could not be presented, so neither the
/// structured result nor the content carries it.
fn withhold_image(response: &mut Value) {
    if let Some(image) = observation_image(response) {
        image.remove("data_base64");
        image.insert("withheld".into(), Value::Bool(true));
    }
}

fn present_response_images(
    response: &mut ServerMessage,
    max_edge: NonZeroU32,
) -> Result<(), String> {
    let observation = match response {
        ServerMessage::Observed { observation, .. } => Some(observation),
        ServerMessage::Acted { result, .. } => result.observation.as_mut(),
        _ => None,
    };
    if let Some(observation) = observation {
        present_observation_image(observation, max_edge)?;
    }
    Ok(())
}

fn present_observation_image(
    observation: &mut Observation,
    max_edge: NonZeroU32,
) -> Result<(), String> {
    let max_edge = max_edge.get();
    if observation.image.mime_type != "image/png" {
        return Err(format!(
            "unsupported observation image type {}",
            observation.image.mime_type
        ));
    }
    let png = BASE64
        .decode(&observation.image.data_base64)
        .map_err(|error| format!("decode controller PNG: {error}"))?;
    let decoded = image::load_from_memory_with_format(&png, ImageFormat::Png)
        .map_err(|error| format!("decode controller PNG pixels: {error}"))?;
    let (source_width, source_height) = decoded.dimensions();
    let decoded_size = PixelSize {
        width: source_width,
        height: source_height,
    };
    if decoded_size != observation.coordinates.source_pixels {
        return Err(format!(
            "controller source dimensions {}x{} do not match decoded PNG {}x{}",
            observation.coordinates.source_pixels.width,
            observation.coordinates.source_pixels.height,
            source_width,
            source_height
        ));
    }

    let presented = if source_width > max_edge || source_height > max_edge {
        decoded.resize(max_edge, max_edge, FilterType::Lanczos3)
    } else {
        decoded
    };
    let (model_width, model_height) = presented.dimensions();
    if model_width != source_width || model_height != source_height {
        let mut png = Cursor::new(Vec::new());
        presented
            .write_to(&mut png, ImageFormat::Png)
            .map_err(|error| format!("encode presented PNG: {error}"))?;
        observation.image.data_base64 = BASE64.encode(png.into_inner());
    }
    observation.coordinates.model_image_pixels = PixelSize {
        width: model_width,
        height: model_height,
    };
    Ok(())
}

fn observation_image(response: &mut Value) -> Option<&mut Map<String, Value>> {
    let observation = if response.get("observation").is_some() {
        response.get_mut("observation")
    } else {
        response
            .get_mut("result")
            .and_then(|result| result.get_mut("observation"))
    };
    observation
        .and_then(|observation| observation.get_mut("image"))
        .and_then(Value::as_object_mut)
}

fn separate_image(mut response: Value) -> (Value, Option<(String, String)>) {
    let image = observation_image(&mut response).and_then(|image| {
        let mime_type = image.remove("mime_type")?.as_str()?.to_owned();
        let data = image.remove("data_base64")?.as_str()?.to_owned();
        image.insert("delivered_as_content".into(), Value::Bool(true));
        Some((mime_type, data))
    });
    (response, image)
}

pub(crate) fn tool_failure(error: ProtocolError) -> Value {
    let code = serde_json::to_value(error.code)
        .expect("error codes serialise")
        .as_str()
        .expect("error codes serialise as strings")
        .to_owned();
    let message = format!("{code}: {}", error.message);
    let structured = json!({
        "type": "error",
        "request_id": null,
        "error": error,
    });
    serde_json::to_value(ToolResult {
        content: vec![json!({ "type": "text", "text": message })],
        structured_content: Some(structured),
        is_error: true,
    })
    .expect("tool failure serialises")
}

pub(crate) fn transport_failure(error: TransportError) -> Value {
    let (message, retryable) = match error {
        TransportError::OutcomeUnknown { reason } => (
            format!(
                "the action may have executed; a fresh Observe is required before deciding what to do next: {reason}"
            ),
            false,
        ),
        error => (error.to_string(), true),
    };
    tool_failure(ProtocolError::backend_failure(message, retryable))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, Rgba};
    use serde_json::json;
    use waldo_protocol::{
        CaptureProvenance, CaptureRoute, CoordinateMapping, FocusState, ImageData, LogicalPoint,
        LogicalRect, Observation, ObservationId, OutputId, OutputTransform, PixelSize,
        SourceSelector,
    };

    use waldo_protocol::{ErrorCode, ProtocolError};

    use super::{present_observation_image, separate_image, tool_failure};

    #[test]
    fn images_are_emitted_once_as_mcp_image_content() {
        let response = json!({
            "type": "observed",
            "observation": {
                "id": 1,
                "image": { "mime_type": "image/png", "data_base64": "cG5n" }
            }
        });

        let (structured, image) = separate_image(response);

        assert_eq!(image, Some(("image/png".into(), "cG5n".into())));
        assert_eq!(
            structured["observation"]["image"]["delivered_as_content"],
            true
        );
        assert!(structured.to_string().find("cG5n").is_none());
    }

    #[test]
    fn tool_errors_retain_the_protocol_error_as_structured_content() {
        let result = tool_failure(ProtocolError {
            code: ErrorCode::InvalidRequest,
            message: "request does not match the protocol schema".into(),
            retryable: false,
        });

        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["error"]["code"],
            "invalid_request"
        );
        assert_eq!(result["structuredContent"]["error"]["retryable"], false);
    }

    fn acted(stop_reason: &str, observation: serde_json::Value) -> waldo_protocol::ServerMessage {
        serde_json::from_value(json!({
            "type": "acted",
            "request_id": 3,
            "result": {
                "driving_session": 1,
                "receipts": [{"index": 0, "status": "failed", "route": null, "error": null}],
                "first_not_executed": null,
                "stop_reason": stop_reason,
                "observation": observation,
                "completion_error": null,
                "verification": [],
                "application_outcome": "inconclusive",
                "completion_evidence": {
                    "frame_state": "unchanged",
                    "settle_deadline_reached": false,
                    "verification_agreement": "not_evaluated"
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_batch_that_did_not_complete_is_a_tool_error() {
        let edge = std::num::NonZeroU32::new(64).unwrap();

        let failed = super::tool_response(acted("execution_failed", json!(null)), edge);
        let completed = super::tool_response(acted("completed", json!(null)), edge);

        assert_eq!(failed["isError"], true);
        assert_eq!(
            failed["structuredContent"]["result"]["receipts"][0]["status"],
            "failed"
        );
        assert!(completed.get("isError").is_none());
    }

    #[test]
    fn a_stop_whose_cleanup_left_state_unrestored_is_a_tool_error() {
        let edge = std::num::NonZeroU32::new(64).unwrap();
        let stopped = |cleanup_limitation: Option<&str>| waldo_protocol::ServerMessage::Stopped {
            request_id: waldo_protocol::RequestId::new(9),
            result: waldo_protocol::StopResult {
                released_session: Some(waldo_protocol::DrivingSessionId::new(3)),
                cleanup_limitation: cleanup_limitation.map(str::to_owned),
            },
        };

        let clean = super::tool_response(stopped(None), edge);
        let unrestored = super::tool_response(
            stopped(Some(
                "clipboard mutation is blocked until exact restoration succeeds",
            )),
            edge,
        );

        assert!(clean.get("isError").is_none());
        assert_eq!(unrestored["isError"], true);
        assert_eq!(
            unrestored["structuredContent"]["result"]["released_session"],
            3
        );
        assert_eq!(
            unrestored["structuredContent"]["result"]["cleanup_limitation"],
            "clipboard mutation is blocked until exact restoration succeeds"
        );
    }

    #[test]
    fn an_unpresentable_image_keeps_the_receipts_and_withholds_only_the_image() {
        let observation = json!({
            "id": 7,
            "captured_at_unix_ms": 1,
            "source": {"type": "output", "output_id": "fixture"},
            "stable_output_id": "fixture",
            "capture": {"route": "full_output", "output_local_bounds": null},
            "image": {"mime_type": "image/png", "data_base64": BASE64.encode("not a png")},
            "coordinates": {
                "source_pixels": {"width": 10, "height": 6},
                "model_image_pixels": {"width": 10, "height": 6},
                "logical_bounds": {"origin": {"x": 0.0, "y": 0.0}, "width": 10.0, "height": 6.0},
                "scale": 1.0,
                "transform": "normal"
            },
            "focus": {"window_id": null, "element": null},
            "accessibility": [],
            "accessibility_available": false
        });

        let result = super::tool_response(
            acted("completed", observation),
            std::num::NonZeroU32::new(64).unwrap(),
        );

        assert_eq!(result["isError"], true);
        let structured = &result["structuredContent"];
        assert_eq!(structured["result"]["receipts"][0]["index"], 0);
        assert_eq!(structured["presentation_error"]["retryable"], false);
        assert_eq!(
            structured["result"]["observation"]["image"]["withheld"],
            true
        );
        assert!(structured.to_string().find("data_base64").is_none());
        assert!(
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|content| content["type"] == "text")
        );
    }

    #[test]
    fn model_image_metadata_matches_the_final_resized_png() {
        let source = ImageBuffer::from_fn(10, 6, |x, y| {
            let cell = (x / 2) + (y / 2) * 5;
            Rgba([cell as u8 * 11, 255 - cell as u8 * 7, cell as u8 * 3, 255])
        });
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(source)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let mut observation = Observation {
            id: ObservationId::new(7),
            captured_at_unix_ms: 1,
            source: SourceSelector::Output {
                output_id: OutputId("fixture".into()),
            },
            stable_output_id: OutputId("fixture".into()),
            capture: CaptureProvenance {
                route: CaptureRoute::FullOutput,
                output_local_bounds: None,
            },
            image: ImageData {
                mime_type: "image/png".into(),
                data_base64: BASE64.encode(encoded.into_inner()),
            },
            coordinates: CoordinateMapping {
                source_pixels: PixelSize {
                    width: 10,
                    height: 6,
                },
                model_image_pixels: PixelSize {
                    width: 10,
                    height: 6,
                },
                logical_bounds: LogicalRect {
                    origin: LogicalPoint { x: 200.0, y: 40.0 },
                    width: 1000.0,
                    height: 600.0,
                },
                scale: 1.0,
                transform: OutputTransform::Normal,
            },
            focus: FocusState {
                window_id: None,
                element: None,
            },
            accessibility: Vec::new(),
            accessibility_available: false,
            accessibility_limitation: None,
            continuation: None,
        };

        present_observation_image(&mut observation, std::num::NonZeroU32::new(5).unwrap()).unwrap();

        let delivered = BASE64.decode(&observation.image.data_base64).unwrap();
        let decoded = image::load_from_memory_with_format(&delivered, ImageFormat::Png).unwrap();
        assert_eq!(decoded.dimensions(), (5, 3));
        assert_eq!(
            observation.coordinates.model_image_pixels,
            PixelSize {
                width: 5,
                height: 3
            }
        );
        assert_eq!(
            observation.coordinates.source_pixels,
            PixelSize {
                width: 10,
                height: 6
            }
        );

        // An independent consumer selecting the centre of delivered cell (3, 1)
        // maps to the same logical target without a hidden correction factor.
        let model_x = 3.5;
        let model_y = 1.5;
        let logical_x = observation.coordinates.logical_bounds.origin.x
            + model_x * observation.coordinates.logical_bounds.width / 5.0;
        let logical_y = observation.coordinates.logical_bounds.origin.y
            + model_y * observation.coordinates.logical_bounds.height / 3.0;
        assert_eq!((logical_x, logical_y), (900.0, 340.0));
    }
}
