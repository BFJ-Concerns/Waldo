use waldo_protocol::{
    ActRequest, Action, ClientMessage, LogicalPoint, ObservationId, PointerButton, RequestId,
    ServerMessage, StopResult, published_schema,
};

#[test]
fn action_documents_round_trip_through_the_shared_wire_types() {
    let message = ClientMessage::Act {
        request_id: RequestId::new(7),
        request: ActRequest {
            observation_id: ObservationId::new(12),
            actions: vec![Action::PointerClick {
                position: LogicalPoint { x: 120.5, y: 42.0 },
                button: PointerButton::Primary,
                count: 1,
            }],
            verification: Vec::new(),
        },
    };

    let json = serde_json::to_string(&message).unwrap();
    let decoded: ClientMessage = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, message);
    assert!(json.contains(r#""type":"act""#));
    assert!(json.contains(r#""type":"pointer_click""#));
}

#[test]
fn unknown_action_variants_fail_closed() {
    let json = r#"
        {
          "type": "act",
          "request_id": 7,
          "request": {
            "observation_id": 12,
            "actions": [{"type": "launch_shell", "command": "nope"}],
            "verification": []
          }
        }
    "#;

    let error = serde_json::from_str::<ClientMessage>(json).unwrap_err();

    assert!(error.to_string().contains("unknown variant"));
}

#[test]
fn additive_result_fields_are_ignored_by_older_readers() {
    let json = r#"
        {
          "type": "stopped",
          "request_id": 8,
          "result": {"released_session": null, "future_field": true},
          "future_envelope_field": "also ignored"
        }
    "#;

    let decoded: ServerMessage = serde_json::from_str(json).unwrap();

    assert_eq!(
        decoded,
        ServerMessage::Stopped {
            request_id: RequestId::new(8),
            result: StopResult {
                released_session: None,
            },
        }
    );
}

#[test]
fn published_schema_names_every_top_level_operation() {
    let schema = published_schema();
    let encoded = serde_json::to_string(&schema).unwrap();

    assert_eq!(schema.schema_major, waldo_protocol::SCHEMA_MAJOR_VERSION);
    for operation in ["hello", "observe", "act", "query", "stop", "handover"] {
        assert!(encoded.contains(operation), "schema omitted {operation}");
    }
    for result in [
        "connected",
        "observed",
        "acted",
        "queried",
        "stopped",
        "handed_over",
        "error",
    ] {
        assert!(encoded.contains(result), "schema omitted {result}");
    }
}
