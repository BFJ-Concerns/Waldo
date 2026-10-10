/// Says where and why a request failed to decode, for a diagnostic returned
/// to whoever sent it. String values the parser quotes are elided, since a
/// request can carry clipboard data or text the operator would not want
/// repeated in an error.
pub fn decode_error_detail(error: &serde_json::Error) -> String {
    let mut detail = String::new();
    let mut quoted = false;
    for character in error.to_string().chars() {
        if character == '"' {
            if !quoted {
                detail.push_str("\"…\"");
            }
            quoted = !quoted;
        } else if !quoted {
            detail.push(character);
        }
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::decode_error_detail;

    #[test]
    fn decode_details_locate_the_fault_without_repeating_values() {
        let error = serde_json::from_str::<crate::ActRequest>(
            r#"{"observation_id": "secret text", "actions": [], "verification": []}"#,
        )
        .unwrap_err();
        let detail = decode_error_detail(&error);

        assert!(detail.contains("line 1"), "{detail}");
        assert!(detail.contains("expected u64"), "{detail}");
        assert!(!detail.contains("secret"), "{detail}");
    }
}
