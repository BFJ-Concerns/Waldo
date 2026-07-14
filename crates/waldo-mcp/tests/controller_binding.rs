use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::oneshot,
};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_mcp::McpServer;
use waldo_niri::{NiriSnapshot, SessionInspector};
use waldo_protocol::{
    Action, ErrorCode, LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform,
    ProtocolError, StopReason,
};
use waldo_screencopy::{Capture, CaptureError, ScreenCapturer};
use waldo_service::ControllerService;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedInspector;

impl SessionInspector for FixedInspector {
    fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        let mut snapshot = NiriSnapshot::unavailable("MCP test session");
        snapshot.outputs = vec![OutputState {
            id: OutputId("DP-1".into()),
            name: "DP-1".into(),
            logical_bounds: LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 1920.0,
                height: 1080.0,
            },
            pixel_size: Some(waldo_protocol::PixelSize {
                width: 1920,
                height: 1080,
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
            png: b"test-png".to_vec(),
            width: 1920,
            height: 1080,
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

struct BlockingExecutor {
    started: mpsc::Sender<()>,
    finished: Arc<AtomicBool>,
}

impl ActionExecutor for BlockingExecutor {
    fn execute(
        &self,
        _action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        let _ = self.started.send(());
        while !cancellation.is_cancelled() {
            std::thread::sleep(Duration::from_millis(2));
        }
        self.finished.store(true, Ordering::Release);
        Err(ActionExecutionFailure::not_executed(ProtocolError {
            code: ErrorCode::Cancelled,
            message: "cancelled by MCP Stop request".into(),
            retryable: true,
        }))
    }
}

fn test_socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "waldo-mcp-test-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
        .join("controller.sock")
}

async fn send(writer: &mut DuplexStream, value: Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn receive(reader: &mut tokio::io::Lines<BufReader<DuplexStream>>) -> Value {
    serde_json::from_str(&reader.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn stop_overtakes_and_quiesces_an_active_mcp_act() {
    let path = test_socket();
    let (started_tx, started_rx) = mpsc::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(BlockingExecutor {
            started: started_tx,
            finished: Arc::clone(&finished),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service_task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));

    let (mut client_writer, server_reader) = tokio::io::duplex(64 * 1024);
    let (server_writer, client_reader) = tokio::io::duplex(512 * 1024);
    let server = McpServer::new(path.clone()).unwrap();
    let mcp_task = tokio::spawn(async move { server.run(server_reader, server_writer).await });
    let mut client_reader = BufReader::new(client_reader).lines();

    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }),
    )
    .await;
    assert_eq!(receive(&mut client_reader).await["id"], 1);

    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "observe",
                "arguments": {
                    "source": { "type": "output", "output_id": "DP-1" },
                    "continue_from": null,
                    "include_accessibility": false
                }
            }
        }),
    )
    .await;
    let observed = receive(&mut client_reader).await;
    assert_eq!(observed["id"], 2);
    assert_eq!(observed["result"]["content"][1]["type"], "image");
    let observation_id = observed["result"]["structuredContent"]["observation"]["id"].clone();

    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "act",
                "arguments": {
                    "observation_id": observation_id,
                    "actions": [{
                        "type": "key_chord",
                        "keys": ["test"]
                    }],
                    "verification": []
                }
            }
        }),
    )
    .await;
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "stop", "arguments": {} }
        }),
    )
    .await;

    let first = receive(&mut client_reader).await;
    let second = receive(&mut client_reader).await;
    let responses = [first, second];
    let acted = responses
        .iter()
        .find(|response| response["id"] == 3)
        .expect("Act response is returned");
    assert_eq!(
        acted["result"]["structuredContent"]["result"]["stop_reason"],
        serde_json::to_value(StopReason::Cancelled).unwrap()
    );
    assert!(responses.iter().any(|response| response["id"] == 4));
    assert!(finished.load(Ordering::Acquire));

    drop(client_writer);
    mcp_task.await.unwrap().unwrap();
    shutdown_tx.send(()).unwrap();
    service_task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}
