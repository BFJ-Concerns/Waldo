use std::io::Cursor;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{GenericImageView, ImageFormat, imageops::FilterType};
use serde_json::{Value, json};
use waldo_protocol::{Observation, PixelSize, ServerMessage};

use crate::tools::ToolResult;

const MAX_MODEL_IMAGE_EDGE: u32 = 2048;

pub(crate) fn tool_response(mut response: ServerMessage) -> Value {
    if let ServerMessage::Error { error, .. } = &response {
        let code = serde_json::to_value(error.code)
            .expect("error codes serialise")
            .as_str()
            .expect("error codes serialise as strings")
            .to_owned();
        return tool_failure(format!("{code}: {}", error.message));
    }

    if let Err(error) = present_response_images(&mut response, MAX_MODEL_IMAGE_EDGE) {
        return tool_failure(format!("image presentation failed: {error}"));
    }

    let response = serde_json::to_value(response).expect("controller responses serialise");
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
        is_error: false,
    })
    .expect("tool result serialises")
}

fn present_response_images(response: &mut ServerMessage, max_edge: u32) -> Result<(), String> {
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

fn present_observation_image(observation: &mut Observation, max_edge: u32) -> Result<(), String> {
    if max_edge == 0 {
        return Err("model image edge limit must be greater than zero".into());
    }
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

fn separate_image(mut response: Value) -> (Value, Option<(String, String)>) {
    let observation = if response.get("observation").is_some() {
        response.get_mut("observation")
    } else {
        response
            .get_mut("result")
            .and_then(|result| result.get_mut("observation"))
    };
    let image = observation
        .and_then(|observation| observation.get_mut("image"))
        .and_then(Value::as_object_mut)
        .and_then(|image| {
            let mime_type = image.remove("mime_type")?.as_str()?.to_owned();
            let data = image.remove("data_base64")?.as_str()?.to_owned();
            image.insert("delivered_as_content".into(), Value::Bool(true));
            Some((mime_type, data))
        });
    (response, image)
}

pub(crate) fn tool_failure(message: String) -> Value {
    serde_json::to_value(ToolResult {
        content: vec![json!({ "type": "text", "text": message })],
        structured_content: None,
        is_error: true,
    })
    .expect("tool failure serialises")
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

    use super::{present_observation_image, separate_image};

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
            cursor: None,
            focus: FocusState {
                window_id: None,
                element: None,
            },
            accessibility: Vec::new(),
            accessibility_available: false,
            continuation: None,
        };

        present_observation_image(&mut observation, 5).unwrap();

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
