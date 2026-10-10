/// Locates a decode failure without repeating any client-supplied value.
/// Parser messages can interpolate values with several quoting conventions,
/// so diagnostics use only the error category and source location.
pub fn decode_error_detail(error: &serde_json::Error) -> String {
    let reason = match error.classify() {
        serde_json::error::Category::Io => "could not read JSON",
        serde_json::error::Category::Syntax => "invalid JSON syntax",
        serde_json::error::Category::Data => "value does not match the protocol schema",
        serde_json::error::Category::Eof => "incomplete JSON",
    };
    format!(
        "{reason} at line {} column {}",
        error.line(),
        error.column()
    )
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
}
