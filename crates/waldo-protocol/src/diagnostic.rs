/// Explains a decode failure without repeating client-supplied values.
/// Expected types, valid names and missing field names describe the schema;
/// unexpected values are elided while the source location is retained.
pub fn decode_error_detail(error: &serde_json::Error) -> String {
    let mut reason = error.to_string();
    // Serde does not escape unknown names inside backticks. Use the final
    // expected-clause delimiter so embedded backticks cannot end redaction.
    for prefix in ["unknown variant `", "unknown field `"] {
        if reason.starts_with(prefix)
            && let Some(end) = reason
                .rfind("`, expected ")
                .into_iter()
                .chain(reason.rfind("`, there are no fields"))
                .max()
        {
            reason.replace_range(prefix.len()..end, "<redacted>");
        }
    }
    // Unexpected numbers and other unquoted values are client data too.
    for prefix in ["invalid type: ", "invalid value: "] {
        if reason.starts_with(prefix)
            && let Some(end) = reason.rfind(", expected ")
        {
            reason.replace_range(prefix.len()..end, "<redacted>");
        }
    }

    let mut detail = String::with_capacity(reason.len());
    let mut quoted = false;
    let mut escaped = false;
    for character in reason.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
                detail.push('"');
            }
        } else if character == '"' {
            quoted = true;
            detail.push_str("\"<redacted>");
        } else {
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
        let documents = [
            r#"{"observation_id":"secret text","actions":[],"verification":[]}"#,
            r#"{"observation_id":"secret\"escaped secret","actions":[],"verification":[]}"#,
            r#"{"observation_id":1,"actions":[{"type":"secret`variant"}],"verification":[]}"#,
            r#"{"observation_id":1,"actions":[],"verification":[],"secret`field":true}"#,
        ];
        for document in documents {
            let error = serde_json::from_str::<crate::ActRequest>(document).unwrap_err();
            let detail = decode_error_detail(&error);
            assert!(detail.contains("line 1"), "{detail}");
            assert!(!detail.contains("secret"), "{detail}");
        }
    }

    #[test]
    fn decode_details_preserve_expected_type() {
        let error = serde_json::from_str::<crate::ActRequest>(
            r#"{"observation_id":"secret\"escaped secret","actions":[],"verification":[]}"#,
        )
        .unwrap_err();
        let detail = decode_error_detail(&error);
        assert!(!detail.contains("secret"), "{detail}");
        assert!(detail.contains("expected u64"), "{detail}");
    }

    #[test]
    fn decode_details_preserve_valid_action_variants() {
        let error = serde_json::from_str::<crate::ActRequest>(
            r#"{"observation_id":1,"actions":[{"type":"secret`variant"}],"verification":[]}"#,
        )
        .unwrap_err();
        let raw = error.to_string();
        let expected = &raw[raw.rfind("expected one of").unwrap()..];
        let detail = decode_error_detail(&error);
        assert!(!detail.contains("secret"), "{detail}");
        assert!(detail.contains(expected), "{detail}");
    }

    #[test]
    fn decode_details_preserve_missing_field_name() {
        let error =
            serde_json::from_str::<crate::ActRequest>(r#"{"observation_id":1,"verification":[]}"#)
                .unwrap_err();
        let detail = decode_error_detail(&error);
        assert!(detail.contains("missing field `actions`"), "{detail}");
    }
}
