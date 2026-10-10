use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tokio::sync::oneshot;
use waldo_action::UnavailableActionExecutor;
use waldo_cli::{Arguments, Command, execute};
use waldo_niri::{NiriSnapshot, SessionInspector, UnavailableInspector};
use waldo_protocol::{
    ClientId, ClientMessage, LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform,
    QueryRequest, RequestId, ServerMessage, StopRequest,
};
use waldo_screencopy::{Capture, CaptureError, ScreenCapturer};
use waldo_service::ControllerService;
use waldo_socket::ControllerClient;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedInspector;

impl SessionInspector for FixedInspector {
    fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        let mut snapshot = NiriSnapshot::unavailable("CLI handover test");
        snapshot.outputs = vec![OutputState {
            id: OutputId("TEST-1".into()),
            name: "TEST-1".into(),
            logical_bounds: LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 64.0,
                height: 64.0,
            },
            pixel_size: Some(waldo_protocol::PixelSize {
                width: 64,
                height: 64,
            }),
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused: true,
        }];
        Ok(snapshot)
    }
}

#[derive(Clone, Copy)]
struct FixedCapturer;

impl ScreenCapturer for FixedCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        Ok(Capture {
            png: b"cli-handover-frame".to_vec(),
            width: 64,
            height: 64,
            captured_at_unix_ms: 1,
        })
    }

    fn capture_region(
        &self,
        output_name: &str,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Result<Capture, CaptureError> {
        self.capture_output(output_name)
    }
}

fn test_socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "waldo-cli-test-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
        .join("controller.sock")
}

#[tokio::test]
async fn query_uses_the_same_controller_schema_end_to_end() {
    let path = test_socket();
    let service = ControllerService::bind_with_inspector(
        &path,
        Duration::from_secs(30),
        Arc::new(UnavailableInspector::new("test session")),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));

    let result = execute(Arguments {
        socket: Some(path.clone()),
        command: Command::Query {
            include_windows: true,
        },
    })
    .await
    .unwrap();

    assert_eq!(result["type"], "queried");
    assert_eq!(result["request_id"], 2);
    assert_eq!(result["result"]["outputs"], serde_json::json!([]));
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn schema_does_not_require_a_running_controller() {
    let result = execute(Arguments {
        socket: Some(PathBuf::from("/definitely/not/a/socket")),
        command: Command::Schema,
    })
    .await
    .unwrap();

    assert_eq!(result["schema_major"], waldo_protocol::SCHEMA_MAJOR_VERSION);
}

#[tokio::test]
async fn invalid_action_document_returns_structured_error_without_a_lease() {
    let path = test_socket();
    let document = path.parent().unwrap().join("invalid-act.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let observe_document = path.parent().unwrap().join("observe.json");
    fs::write(
        &observe_document,
        r#"{"source": {"type": "output", "output_id": "TEST-1"}, "include_accessibility": false}"#,
    )
    .unwrap();
    fs::write(
        &document,
        r#"{
            "observation_id": 1,
            "actions": [{"type": "future_action"}],
            "verification": []
        }"#,
    )
    .unwrap();
    let service = ControllerService::bind_with_inspector(
        &path,
        Duration::from_secs(30),
        Arc::new(UnavailableInspector::new("test session")),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));

    let result = execute(Arguments {
        socket: Some(path.clone()),
        command: Command::Act {
            document,
            observe: observe_document,
            stop_after_ms: None,
            handover_to: None,
        },
    })
    .await
    .unwrap();
    assert_eq!(result["type"], "error");
    assert_eq!(result["error"]["code"], "invalid_request");
    assert_eq!(result["error"]["retryable"], false);

    let mut observer = ControllerClient::connect(&path, ClientId::new("lease-oracle").unwrap())
        .await
        .unwrap();
    let query = observer
        .request(&ClientMessage::Query {
            request_id: RequestId::new(2),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = query else {
        panic!("lease oracle Query failed")
    };
    assert_eq!(result.driving_session, None);

    drop(observer);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn cli_act_can_transfer_its_owned_lease_to_a_connected_controller_client() {
    let path = test_socket();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let observe_document = path.parent().unwrap().join("observe.json");
    let act_document = path.parent().unwrap().join("act.json");
    fs::write(
        &observe_document,
        r#"{
            "source": {"type": "output", "output_id": "TEST-1"},
            "continue_from": null,
            "include_accessibility": false
        }"#,
    )
    .unwrap();
    fs::write(
        &act_document,
        r#"{
            "observation_id": 0,
            "actions": [{"type": "capture", "source": {"type": "output", "output_id": "TEST-1"}}],
            "verification": []
        }"#,
    )
    .unwrap();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(UnavailableActionExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut target = ControllerClient::connect(&path, ClientId::new("handover-target").unwrap())
        .await
        .unwrap();

    let result = execute(Arguments {
        socket: Some(path.clone()),
        command: Command::Act {
            document: act_document,
            observe: observe_document,
            stop_after_ms: None,
            handover_to: Some(target.connection_id().get()),
        },
    })
    .await
    .unwrap();
    assert_eq!(result["type"], "act_handover");
    assert_eq!(result["act"]["type"], "acted");
    assert_eq!(result["handover"]["type"], "handed_over");
    assert_eq!(
        result["handover"]["result"]["owner"],
        target.connection_id().get()
    );

    let query = target
        .request(&ClientMessage::Query {
            request_id: RequestId::new(2),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = query else {
        panic!("target Query failed")
    };
    assert_eq!(result.driving_connection, Some(target.connection_id()));
    assert!(result.driving_session.is_some());
    let stopped = target
        .request(&ClientMessage::Stop {
            request_id: RequestId::new(3),
            request: StopRequest::default(),
        })
        .await
        .unwrap();
    assert!(matches!(stopped, ServerMessage::Stopped { .. }));

    drop(target);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

/// Capture succeeds for grounding; a cancelled execution then makes the
/// independent post-action capture unavailable.
struct CompletionCapturer {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl ScreenCapturer for CompletionCapturer {
    fn available(&self) -> bool {
        true
    }
    fn capture_output(&self, output: &str) -> Result<Capture, CaptureError> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(CaptureError::CompositorFailed)
        } else {
            FixedCapturer.capture_output(output)
        }
    }
    fn capture_region(
        &self,
        output: &str,
        _x: i32,
        _y: i32,
        _w: i32,
        _h: i32,
    ) -> Result<Capture, CaptureError> {
        self.capture_output(output)
    }
}

struct CancellableExecutor {
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl waldo_action::ActionExecutor for CancellableExecutor {
    fn execute(
        &self,
        _action: &waldo_protocol::Action,
        cancellation: &waldo_controller::ExecutionToken,
    ) -> Result<waldo_action::ActionExecution, waldo_action::ActionExecutionFailure> {
        while !cancellation.is_cancelled() {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.cancelled.store(true, Ordering::Release);
        Err(waldo_action::ActionExecutionFailure::not_executed(
            waldo_protocol::ProtocolError::cancelled("cancelled during execution"),
        ))
    }
    fn supports(&self, route: waldo_protocol::ActionRoute) -> bool {
        route == waldo_protocol::ActionRoute::VirtualKeyboard
    }
}

#[tokio::test]
async fn binary_cancellation_with_failed_completion_exits_unsuccessfully() {
    let path = test_socket();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let observe = path.parent().unwrap().join("observe.json");
    let act = path.parent().unwrap().join("act.json");
    fs::write(
        &observe,
        r#"{"source":{"type":"output","output_id":"TEST-1"},"include_accessibility":false}"#,
    )
    .unwrap();
    fs::write(&act, r#"{"observation_id":0,"actions":[{"type":"key_chord","keys":["Return"]}],"verification":[]}"#).unwrap();
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(CompletionCapturer {
            cancelled: cancelled.clone(),
        }),
        Arc::new(CancellableExecutor { cancelled }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_waldo"))
        .arg("--socket")
        .arg(&path)
        .arg("act")
        .arg(&act)
        .arg("--observe")
        .arg(&observe)
        .args(["--stop-after-ms", "100"])
        .output()
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["act"]["result"]["stop_reason"], "cancelled",
        "{value}"
    );
    assert_eq!(
        value["act"]["result"]["completion_error"]["code"], "backend_failure",
        "{value}"
    );
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
    assert!(
        !output.status.success(),
        "CLI reported success over a failed capture: {value}"
    );
}

#[tokio::test]
async fn binary_schema_matches_the_published_wire_contract() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_waldo"))
        .arg("schema")
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        schema,
        serde_json::to_value(waldo_protocol::published_schema()).unwrap()
    );
    assert!(
        !schema["server_messages"]
            .to_string()
            .contains("CursorState")
    );
    assert!(
        schema["client_messages"]
            .to_string()
            .contains("Reserved for wire compatibility")
    );
}

#[tokio::test]
async fn binary_rejects_the_unused_client_identity_option() {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_waldo"))
        .args(["--client-id", "custom", "schema"])
        .output()
        .await
        .unwrap();
    assert!(
        !output.status.success(),
        "unused identity option was accepted"
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unexpected argument '--client-id'")
    );
}

#[tokio::test]
async fn binary_document_errors_do_not_repeat_client_values() {
    use tokio::io::AsyncWriteExt;
    let path = test_socket();
    let service = ControllerService::bind_with_inspector(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    for document in [
        r#"{"type":"act","request_id":2,"request":{"observation_id":"secret\"escaped secret","actions":[],"verification":[]}}"#,
        r#"{"type":"secret`variant","request_id":2}"#,
        r#"{"type":"stop","request_id":2,"request":{"secret`field":true}}"#,
    ] {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_waldo"))
            .arg("--socket")
            .arg(&path)
            .args(["send", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(document.as_bytes())
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "invalid_request");
        assert!(!output.status.success());
        assert!(!value.to_string().contains("secret"), "{value}");
    }
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}
