use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::oneshot,
};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_cli::{Arguments, Command, execute};
use waldo_controller::ExecutionToken;
use waldo_mcp::McpServer;
use waldo_niri::{NiriSnapshot, SessionInspector};
use waldo_protocol::{
    Action, ActionOutput, ActionRoute, ClipboardPreservationId, LogicalPoint, LogicalRect,
    OutputId, OutputState, OutputTransform, SCHEMA_MAJOR_VERSION, TextRoutePreference,
    WindowCaptureCapability, WindowId, WindowState, WorkspaceId, WorkspaceState,
};
use waldo_screencopy::{Capture, CaptureError, ScreenCapturer};
use waldo_service::ControllerService;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize)]
struct Corpus {
    schema_major: u16,
    documents: Vec<CorpusDocument>,
    deferred: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CorpusDocument {
    name: String,
    file: String,
    families: Vec<String>,
    stop_after_ms: Option<u64>,
}

#[derive(Default)]
struct ConsumerState {
    actions: Vec<Value>,
}

struct RecordingExecutor {
    state: Arc<Mutex<ConsumerState>>,
}

impl ActionExecutor for RecordingExecutor {
    fn execute(
        &self,
        action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.state
            .lock()
            .unwrap()
            .actions
            .push(serde_json::to_value(action).unwrap());
        let route = match action {
            Action::PointerMoveAbsolute { .. }
            | Action::PointerMoveRelative { .. }
            | Action::PointerButton { .. }
            | Action::PointerClick { .. }
            | Action::PointerDrag { .. }
            | Action::PointerPath { .. }
            | Action::Scroll { .. } => ActionRoute::VirtualPointer,
            Action::Key { .. } | Action::KeyChord { .. } => ActionRoute::VirtualKeyboard,
            Action::Text { route, .. } => match route {
                TextRoutePreference::Accessibility | TextRoutePreference::Automatic => {
                    ActionRoute::Accessibility
                }
                TextRoutePreference::ClipboardPaste => ActionRoute::ClipboardPaste,
                TextRoutePreference::VirtualKeyboard => ActionRoute::VirtualKeyboard,
            },
            Action::Accessibility { .. } => ActionRoute::Accessibility,
            Action::ClipboardRead { .. }
            | Action::ClipboardWrite { .. }
            | Action::ClipboardWriteOffer { .. }
            | Action::ClipboardPreserve
            | Action::ClipboardRestore { .. }
            | Action::ClipboardKeep { .. } => ActionRoute::Clipboard,
            Action::Window { .. } | Action::Workspace { .. } => ActionRoute::NiriIpc,
            Action::Wait { .. } | Action::Capture { .. } => unreachable!("controller-owned action"),
        };
        let output = match action {
            Action::ClipboardRead { mime_type } => Some(ActionOutput::Clipboard {
                mime_type: mime_type.clone(),
                data_base64: "cGFyaXR5".into(),
            }),
            Action::ClipboardPreserve => Some(ActionOutput::ClipboardPreservation {
                preservation_id: ClipboardPreservationId::new(1),
            }),
            _ => None,
        };
        Ok(ActionExecution { route, output })
    }

    fn supports(&self, _route: ActionRoute) -> bool {
        true
    }
}

#[derive(Clone, Copy)]
struct ParityInspector;

impl SessionInspector for ParityInspector {
    fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        let mut snapshot = NiriSnapshot::unavailable("binding parity fixture");
        snapshot.outputs = vec![OutputState {
            id: OutputId("PARITY-1".into()),
            name: "PARITY-1".into(),
            logical_bounds: LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 800.0,
                height: 600.0,
            },
            pixel_size: Some(waldo_protocol::PixelSize {
                width: 80,
                height: 60,
            }),
            scale: 1.0,
            transform: OutputTransform::Normal,
            focused: true,
        }];
        snapshot.workspaces = vec![WorkspaceState {
            id: WorkspaceId::new(1),
            index: 1,
            name: Some("parity".into()),
            output_id: Some(OutputId("PARITY-1".into())),
            active: true,
            focused: true,
        }];
        snapshot.windows = vec![WindowState {
            id: WindowId("42".into()),
            title: Some("Binding parity fixture".into()),
            application_id: Some("waldo-parity".into()),
            pid: None,
            logical_bounds: Some(LogicalRect {
                origin: LogicalPoint { x: 0.0, y: 0.0 },
                width: 800.0,
                height: 600.0,
            }),
            is_floating: true,
            capture: WindowCaptureCapability {
                available: true,
                limitation: None,
            },
            focused: true,
            workspace_id: Some(WorkspaceId::new(1)),
            output_id: Some(OutputId("PARITY-1".into())),
        }];
        snapshot.focused_window = Some(WindowId("42".into()));
        Ok(snapshot)
    }
}

struct StateCapturer {
    state: Arc<Mutex<ConsumerState>>,
}

impl StateCapturer {
    fn capture(&self) -> Capture {
        let value = self.state.lock().unwrap().actions.len() as u8;
        let pixels = ImageBuffer::from_pixel(80, 60, Rgba([value, 0x51, 0x9b, 0xff]));
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels)
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        Capture {
            png: png.into_inner(),
            width: 80,
            height: 60,
            captured_at_unix_ms: u64::from(value) + 1,
        }
    }
}

impl ScreenCapturer for StateCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        Ok(self.capture())
    }

    fn capture_region(
        &self,
        _output_name: &str,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Result<Capture, CaptureError> {
        Ok(self.capture())
    }
}

struct Harness {
    path: PathBuf,
    state: Arc<Mutex<ConsumerState>>,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), waldo_service::ServiceError>>,
}

impl Harness {
    async fn start() -> Self {
        let path = std::env::temp_dir()
            .join(format!(
                "waldo-binding-parity-{}-{}",
                std::process::id(),
                NEXT_PATH.fetch_add(1, Ordering::Relaxed)
            ))
            .join("controller.sock");
        let state = Arc::new(Mutex::new(ConsumerState::default()));
        let service = ControllerService::bind_with_all_backends(
            &path,
            Duration::from_secs(30),
            Arc::new(ParityInspector),
            Arc::new(StateCapturer {
                state: Arc::clone(&state),
            }),
            Arc::new(RecordingExecutor {
                state: Arc::clone(&state),
            }),
        )
        .unwrap();
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(service.run_until(async {
            let _ = shutdown_rx.await;
        }));
        Self {
            path,
            state,
            shutdown,
            task,
        }
    }

    fn actions(&self) -> Vec<Value> {
        self.state.lock().unwrap().actions.clone()
    }

    async fn stop(self) {
        let parent = self.path.parent().unwrap().to_owned();
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(parent);
    }
}

struct McpClient {
    input: DuplexStream,
    output: tokio::io::Lines<BufReader<DuplexStream>>,
    task: tokio::task::JoinHandle<Result<(), waldo_mcp::McpError>>,
}

impl McpClient {
    async fn start(path: PathBuf) -> Self {
        let (input, server_reader) = tokio::io::duplex(64 * 1024);
        let (server_writer, output) = tokio::io::duplex(2 * 1024 * 1024);
        let server = McpServer::new(path);
        let task = tokio::spawn(async move { server.run(server_reader, server_writer).await });
        let mut client = Self {
            input,
            output: BufReader::new(output).lines(),
            task,
        };
        let _ = client
            .call(1, "initialize", json!({"protocolVersion": "2025-06-18"}))
            .await;
        client
    }

    async fn send(&mut self, id: u64, method: &str, params: Value) {
        self.input
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    async fn receive(&mut self) -> Value {
        serde_json::from_str(&self.output.next_line().await.unwrap().unwrap()).unwrap()
    }

    async fn call(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(id, method, params).await;
        self.receive().await
    }

    async fn stop(self) {
        drop(self.input);
        self.task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn saved_v2_corpus_has_stable_cli_mcp_semantics_and_final_consumers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/v2");
    let corpus: Corpus =
        serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(corpus.schema_major, SCHEMA_MAJOR_VERSION);
    assert_eq!(corpus.deferred, ["mcp-handover"]);
    assert!(
        corpus
            .documents
            .iter()
            .all(|document| !document.families.is_empty())
    );

    for document in corpus.documents {
        let cli_harness = Harness::start().await;
        let cli_result = execute(Arguments {
            socket: Some(cli_harness.path.clone()),
            command: Command::Act {
                document: root.join(&document.file),
                observe: root.join("observe.json"),
                stop_after_ms: document.stop_after_ms,
                handover_to: None,
            },
        })
        .await
        .unwrap();
        let cli_actions = cli_harness.actions();

        let mcp_harness = Harness::start().await;
        let mut mcp = McpClient::start(mcp_harness.path.clone()).await;
        let observe: Value =
            serde_json::from_slice(&fs::read(root.join("observe.json")).unwrap()).unwrap();
        let observed = mcp
            .call(
                2,
                "tools/call",
                json!({"name": "observe", "arguments": observe}),
            )
            .await;
        let observation_id = observed["result"]["structuredContent"]["observation"]["id"].clone();
        let mut request: Value =
            serde_json::from_slice(&fs::read(root.join(&document.file)).unwrap()).unwrap();
        request["observation_id"] = observation_id;

        let (mcp_act, mcp_stop) = if let Some(delay_ms) = document.stop_after_ms {
            mcp.send(
                3,
                "tools/call",
                json!({"name": "act", "arguments": request}),
            )
            .await;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            mcp.send(4, "tools/call", json!({"name": "stop", "arguments": {}}))
                .await;
            let first = mcp.receive().await;
            let second = mcp.receive().await;
            let (act, stop) = if first["id"] == 3 {
                (first, second)
            } else {
                (second, first)
            };
            (act, Some(stop))
        } else {
            (
                mcp.call(
                    3,
                    "tools/call",
                    json!({"name": "act", "arguments": request}),
                )
                .await,
                None,
            )
        };
        let mcp_actions = mcp_harness.actions();

        let mut cli_act = if document.stop_after_ms.is_some() {
            cli_result["act"].clone()
        } else {
            cli_result.clone()
        };
        let mut mcp_act = mcp_act["result"]["structuredContent"].clone();
        normalise(&mut cli_act);
        normalise(&mut mcp_act);
        assert_eq!(cli_act, mcp_act, "{} result semantics", document.name);
        assert_eq!(cli_actions, mcp_actions, "{} final consumer", document.name);

        if let Some(mut mcp_stop) = mcp_stop {
            let mut cli_stop = cli_result["stop"].clone();
            mcp_stop = mcp_stop["result"]["structuredContent"].clone();
            normalise(&mut cli_stop);
            normalise(&mut mcp_stop);
            assert_eq!(cli_stop, mcp_stop, "{} Stop semantics", document.name);
            assert!(cli_actions.is_empty(), "stopped tail reached CLI consumer");
        }

        mcp.stop().await;
        cli_harness.stop().await;
        mcp_harness.stop().await;
    }
}

fn normalise(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalise),
        Value::Object(object) => {
            object.remove("request_id");
            object.remove("captured_at_unix_ms");
            for field in [
                "driving_session",
                "released_session",
                "previous_session",
                "driving_connection",
                "owner",
            ] {
                if let Some(value) = object.get_mut(field) {
                    *value = Value::Bool(!value.is_null());
                }
            }
            if object.contains_key("image") && object.contains_key("coordinates") {
                object.remove("id");
                object.remove("image");
            }
            object.values_mut().for_each(normalise);
        }
        _ => {}
    }
}
