use waldo_protocol::{
    ActRequest, Action, ClientMessage, ClipboardPreservationId, ClipboardRepresentation,
    LogicalPoint, ObservationId, PointerButton, RequestId, ServerMessage, StopResult,
    published_schema,
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
fn atomic_clipboard_offer_is_additive_to_the_single_representation_action() {
    let old = Action::ClipboardWrite {
        mime_type: "text/plain".into(),
        data_base64: "cGxhaW4=".into(),
    };
    let offer = Action::ClipboardWriteOffer {
        representations: vec![
            ClipboardRepresentation {
                mime_type: "text/html".into(),
                data_base64: "PGI+aHRtbDwvYj4=".into(),
            },
            ClipboardRepresentation {
                mime_type: "text/plain".into(),
                data_base64: "cGxhaW4=".into(),
            },
        ],
    };

    for action in [old, offer] {
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }
}

#[test]
fn clipboard_keep_is_additive_to_the_preservation_actions() {
    let actions = [
        Action::ClipboardPreserve,
        Action::ClipboardRestore {
            preservation_id: ClipboardPreservationId::new(7),
        },
        Action::ClipboardKeep {
            preservation_id: ClipboardPreservationId::new(7),
        },
    ];

    for action in actions {
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }
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
fn unsupported_fields_inside_requests_are_refused() {
    // Each valid request, and the field added to it that it does not take.
    let cases = [
        // A selector mixing two sources' identities.
        (
            r#"{"type":"observe","request_id":2,"request":{"source":{"type":"output","output_id":"DP-1"},"include_accessibility":false}}"#,
            r#""output_id":"DP-1""#,
            r#""output_id":"DP-1","window_id":"7""#,
        ),
        // An action parameter.
        (
            r#"{"type":"act","request_id":3,"request":{"observation_id":1,"actions":[{"type":"key","key":"Return","state":"pressed"}],"verification":[]}}"#,
            r#""state":"pressed""#,
            r#""state":"pressed","repeat":3"#,
        ),
        // A predicate parameter.
        (
            r#"{"type":"act","request_id":4,"request":{"observation_id":1,"actions":[],"verification":[{"type":"process_exited","pid":4}]}}"#,
            r#""pid":4"#,
            r#""pid":4,"signal":9"#,
        ),
        // An envelope field outside the message's own.
        (
            r#"{"type":"stop","request_id":5,"request":{}}"#,
            r#""request":{}"#,
            r#""request":{},"force":true"#,
        ),
    ];
    for (valid, field, extended) in cases {
        serde_json::from_str::<ClientMessage>(valid)
            .unwrap_or_else(|error| panic!("{valid}: {error}"));
        let refused = valid.replacen(field, extended, 1);
        assert_ne!(refused, valid);
        assert!(
            serde_json::from_str::<ClientMessage>(&refused).is_err(),
            "accepted {refused}"
        );
    }
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
                cleanup_limitation: None,
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
    for evidence in [
        "application_outcome",
        "completion_evidence",
        "frame_state",
        "settle_deadline_reached",
        "verification_agreement",
    ] {
        assert!(encoded.contains(evidence), "schema omitted {evidence}");
    }
    let schema = serde_json::to_value(schema).unwrap();
    for removed in ["\"cursor\"", "CursorState"] {
        assert!(!encoded.contains(removed), "schema advertises {removed}");
    }
    let client = schema["client_messages"].to_string();
    assert!(client.contains("Reserved for wire compatibility; unread by the controller"));
    let result = &schema["server_messages"]["$defs"]["ActResult"];
    let description = result["properties"]["observation"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("bounded sampling"), "{description}");
    assert!(description.contains("completion_evidence"), "{description}");
    assert_eq!(schema["schema_major"], 2);
}
