use serde_json::json;
use waldo_verification::complete_fixture_events;

#[test]
fn fixture_event_consumer_preserves_optional_digest_validation() {
    let event = json!({
        "schema_version": 1,
        "fixture_version": "1",
        "toolkit": "gtk",
        "display_route": "native_wayland",
        "seed": 42,
        "sequence": 1,
        "recorded_at_unix_ns": 1,
        "kind": "text_entered",
        "control": "text-entry",
        "x": null,
        "y": null,
        "button": null,
        "horizontal_delta": null,
        "vertical_delta": null,
        "physical_key": null,
        "logical_key_sha256": null,
        "modifiers": [],
        "text_length": 0,
        "text_sha256": null
    });
    for field in ["logical_key_sha256", "text_sha256"] {
        for digest in [None, Some("a".repeat(64)), Some("A".repeat(64))] {
            let mut record = event.clone();
            record[field] = json!(digest);
            let input = format!("{record}\n");
            let decoded = complete_fixture_events(&input).unwrap();
            assert_eq!(decoded.len(), 1);
            assert_eq!(
                serde_json::to_value(&decoded[0]).unwrap()[field],
                record[field]
            );
        }
        for digest in [
            "a".repeat(63),
            "a".repeat(65),
            "g".repeat(64),
            "é".repeat(32),
        ] {
            let mut record = event.clone();
            record[field] = json!(digest);
            let input = format!("{record}\n");
            let error = complete_fixture_events(&input).unwrap_err().to_string();
            assert_eq!(
                error,
                format!("{field} must be a 64-character hexadecimal SHA-256 digest")
            );
        }
    }
}
