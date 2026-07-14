use serde_json::{Value, json};
use waldo_protocol::ServerMessage;

use crate::tools::ToolResult;

pub(crate) fn tool_response(response: ServerMessage) -> Value {
    if let ServerMessage::Error { error, .. } = &response {
        let code = serde_json::to_value(error.code)
            .expect("error codes serialise")
            .as_str()
            .expect("error codes serialise as strings")
            .to_owned();
        return tool_failure(format!("{code}: {}", error.message));
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
    use serde_json::json;

    use super::separate_image;

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
}
