use std::{
    fs,
    io::Cursor,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, Rgba};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
struct GridInspector;

impl SessionInspector for GridInspector {
    fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        let mut snapshot = NiriSnapshot::unavailable("MCP grid session");
        snapshot.outputs = vec![OutputState {
            id: OutputId("GRID-1".into()),
            name: "GRID-1".into(),
            logical_bounds: LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 2560.0,
                height: 1440.0,
            },
            pixel_size: Some(waldo_protocol::PixelSize {
                width: 2560,
                height: 1440,
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

fn encoded_image(width: u32, height: u32, pixel: impl Fn(u32, u32) -> Rgba<u8>) -> Vec<u8> {
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(ImageBuffer::from_fn(width, height, pixel))
        .write_to(&mut png, ImageFormat::Png)
        .unwrap();
    png.into_inner()
}

impl ScreenCapturer for FixedCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        Ok(Capture {
            png: encoded_image(4, 2, |x, y| Rgba([x as u8, y as u8, 0x80, 0xff])),
            width: 4,
            height: 2,
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

struct GridCapturer {
    before: Vec<u8>,
    after: Vec<u8>,
    calls: AtomicUsize,
}

impl ScreenCapturer for GridCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Capture {
            png: if call == 0 {
                self.before.clone()
            } else {
                self.after.clone()
            },
            width: 2560,
            height: 1440,
            captured_at_unix_ms: call as u64 + 1,
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

struct RecordingExecutor {
    actions: Arc<Mutex<Vec<Action>>>,
}

impl ActionExecutor for RecordingExecutor {
    fn execute(
        &self,
        action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.actions.lock().unwrap().push(action.clone());
        Ok(waldo_protocol::ActionRoute::VirtualPointer.into())
    }

    fn supports(&self, route: waldo_protocol::ActionRoute) -> bool {
        route == waldo_protocol::ActionRoute::VirtualPointer
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

async fn send(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn receive(
    reader: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
) -> Value {
    serde_json::from_str(&reader.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test]
async fn invalid_tool_arguments_are_structured_and_do_not_touch_controller_state() {
    let path = test_socket();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(RecordingExecutor {
            actions: Arc::clone(&recorded),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service_task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));

    let (mut client_writer, server_reader) = tokio::io::duplex(64 * 1024);
    let (server_writer, client_reader) = tokio::io::duplex(128 * 1024);
    let server = McpServer::new(path.clone()).unwrap();
    let mcp_task = tokio::spawn(async move { server.run(server_reader, server_writer).await });
    let mut client_reader = BufReader::new(client_reader).lines();

    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18"}
        }),
    )
    .await;
    let _ = receive(&mut client_reader).await;

    for (id, name, arguments) in [
        (
            2,
            "query",
            json!({"include_windows": false, "future_field": true}),
        ),
        (
            3,
            "act",
            json!({
                "observation_id": 1,
                "actions": [{"type": "future_action"}],
                "verification": []
            }),
        ),
        (4, "stop", json!({"unexpected": true})),
    ] {
        send(
            &mut client_writer,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }),
        )
        .await;
        let response = receive(&mut client_reader).await;
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["structuredContent"]["error"]["code"],
            "invalid_request"
        );
        assert_eq!(
            response["result"]["structuredContent"]["error"]["retryable"],
            false
        );
    }

    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "query", "arguments": {"include_windows": false}}
        }),
    )
    .await;
    let queried = receive(&mut client_reader).await;
    assert_eq!(queried["id"], 5);
    assert_eq!(
        queried["result"]["structuredContent"]["result"]["driving_session"],
        Value::Null
    );
    assert!(recorded.lock().unwrap().is_empty());

    drop(client_writer);
    mcp_task.await.unwrap().unwrap();
    shutdown_tx.send(()).unwrap();
    service_task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
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
    assert_eq!(
        acted["result"]["structuredContent"]["result"]["application_outcome"],
        "inconclusive"
    );
    assert!(
        acted["result"]["structuredContent"]["result"]["completion_evidence"]["frame_state"]
            .is_string()
    );
    assert!(responses.iter().any(|response| response["id"] == 4));
    assert!(finished.load(Ordering::Acquire));

    drop(client_writer);
    mcp_task.await.unwrap().unwrap();
    shutdown_tx.send(()).unwrap();
    service_task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn resized_model_grid_maps_to_the_exact_final_pointer_consumer() {
    let path = test_socket();
    let colour = |x: u32, y: u32| {
        let column = x / 512;
        let row = y / 480;
        let cell = column + row * 5;
        Rgba([
            (cell * 29 + 17) as u8,
            (cell * 13 + 61) as u8,
            (cell * 7 + 101) as u8,
            0xff,
        ])
    };
    let before = encoded_image(2560, 1440, colour);
    let after = encoded_image(2560, 1440, |x, y| {
        if x == 0 && y == 0 {
            Rgba([0xff, 0xff, 0xff, 0xff])
        } else {
            colour(x, y)
        }
    });
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(GridInspector),
        Arc::new(GridCapturer {
            before,
            after,
            calls: AtomicUsize::new(0),
        }),
        Arc::new(RecordingExecutor {
            actions: Arc::clone(&recorded),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service_task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let (mut client_writer, server_reader) = tokio::io::duplex(64 * 1024);
    let (server_writer, client_reader) = tokio::io::duplex(2 * 1024 * 1024);
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
    let _ = receive(&mut client_reader).await;
    send(
        &mut client_writer,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "observe",
                "arguments": {
                    "source": { "type": "output", "output_id": "GRID-1" },
                    "continue_from": null,
                    "include_accessibility": false
                }
            }
        }),
    )
    .await;
    let observed = receive(&mut client_reader).await;
    let delivered_png = BASE64
        .decode(observed["result"]["content"][1]["data"].as_str().unwrap())
        .unwrap();
    let delivered = image::load_from_memory_with_format(&delivered_png, ImageFormat::Png).unwrap();
    assert_eq!(delivered.dimensions(), (2048, 1152));
    let mapping = &observed["result"]["structuredContent"]["observation"]["coordinates"];
    assert_eq!(
        mapping["source_pixels"],
        json!({"width": 2560, "height": 1440})
    );
    assert_eq!(
        mapping["model_image_pixels"],
        json!({"width": 2048, "height": 1152})
    );

    let model_x = 3.5 * 2048.0 / 5.0;
    let model_y = 1.5 * 1152.0 / 3.0;
    let target = LogicalPoint {
        x: model_x * 2560.0 / 2048.0,
        y: model_y * 1440.0 / 1152.0,
    };
    let sampled = delivered
        .to_rgba8()
        .get_pixel(model_x as u32, model_y as u32)
        .0;
    assert_eq!(sampled, colour(3 * 512 + 256, 480 + 240).0);
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
                        "type": "pointer_click",
                        "position": {"x": target.x, "y": target.y},
                        "button": "primary",
                        "count": 1
                    }],
                    "verification": []
                }
            }
        }),
    )
    .await;
    let acted = receive(&mut client_reader).await;
    assert_eq!(acted["id"], 3);
    assert_eq!(
        recorded.lock().unwrap().as_slice(),
        &[Action::PointerClick {
            position: target,
            button: waldo_protocol::PointerButton::Primary,
            count: 1,
        }]
    );

    drop(client_writer);
    mcp_task.await.unwrap().unwrap();
    shutdown_tx.send(()).unwrap();
    service_task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn binary_resolves_image_edge_settings_and_reports_delivered_dimensions() {
    let directory =
        std::env::temp_dir().join(format!("waldo-mcp-image-edge-{}", std::process::id()));
    fs::create_dir_all(directory.join("waldo")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        directory.join("waldo/settings"),
        "WALDO_MODEL_IMAGE_MAX_EDGE=640\n",
    )
    .unwrap();
    let path = directory.join("controller.sock");
    let png = encoded_image(2560, 1440, |_, _| Rgba([17, 61, 101, 255]));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(GridInspector),
        Arc::new(GridCapturer {
            before: png.clone(),
            after: png,
            calls: AtomicUsize::new(0),
        }),
        Arc::new(RecordingExecutor {
            actions: Arc::new(Mutex::new(Vec::new())),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service_task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    for (environment, expected) in [
        (None, (640, 360)),
        (Some("1280"), (1280, 720)),
        (Some("3000"), (2560, 1440)),
    ] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_waldo-mcp"));
        command
            .env("XDG_CONFIG_HOME", &directory)
            .env("WALDO_SOCKET_PATH", &path)
            .env_remove("WALDO_MODEL_IMAGE_MAX_EDGE")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(value) = environment {
            command.env("WALDO_MODEL_IMAGE_MAX_EDGE", value);
        }
        let mut child = command.spawn().unwrap();
        let mut writer = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap()).lines();
        send(&mut writer, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).await;
        assert!(receive(&mut reader).await["result"]["protocolVersion"].is_string());
        send(
            &mut writer,
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .await;
        assert_eq!(
            receive(&mut reader).await["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        send(&mut writer, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"observe","arguments":{"source":{"type":"output","output_id":"GRID-1"},"continue_from":null,"include_accessibility":false}}})).await;
        let response = receive(&mut reader).await;
        let png = BASE64
            .decode(response["result"]["content"][1]["data"].as_str().unwrap())
            .unwrap();
        let delivered = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
        assert_eq!(
            delivered.dimensions(),
            expected,
            "setting must reach binary image presentation"
        );
        let coordinates = &response["result"]["structuredContent"]["observation"]["coordinates"];
        let mapping: waldo_protocol::CoordinateMapping =
            serde_json::from_value(coordinates.clone()).unwrap();
        let image_centre = LogicalPoint {
            x: f64::from(delivered.width()) / 2.0,
            y: f64::from(delivered.height()) / 2.0,
        };
        let logical_centre = LogicalPoint {
            x: mapping.logical_bounds.origin.x
                + image_centre.x * mapping.logical_bounds.width
                    / f64::from(mapping.model_image_pixels.width),
            y: mapping.logical_bounds.origin.y
                + image_centre.y * mapping.logical_bounds.height
                    / f64::from(mapping.model_image_pixels.height),
        };
        assert_eq!(
            logical_centre,
            LogicalPoint {
                x: 1280.0,
                y: 720.0
            },
            "delivered image centre must map to the logical capture centre"
        );
        assert_eq!(
            coordinates["source_pixels"],
            json!({"width":2560,"height":1440})
        );
        assert_eq!(
            coordinates["model_image_pixels"],
            json!({"width":expected.0,"height":expected.1})
        );
        drop(writer);
        assert!(child.wait().await.unwrap().success());
    }
    shutdown_tx.send(()).unwrap();
    service_task.await.unwrap().unwrap();
    fs::remove_dir_all(directory).unwrap();
}
