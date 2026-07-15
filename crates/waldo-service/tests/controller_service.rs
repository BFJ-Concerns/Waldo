use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::mpsc,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tokio::{io::AsyncWriteExt, sync::oneshot};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_niri::{NiriSnapshot, SessionInspector, UnavailableInspector};
use waldo_protocol::{
    Action, ActionRoute, ClientId, ClientMessage, ErrorCode, LogicalPoint, LogicalRect,
    ObserveRequest, OutputId, OutputState, OutputTransform, Predicate, ProtocolError, QueryRequest,
    RequestId, SCHEMA_MAJOR_VERSION, ServerMessage, SourceSelector, StopReason, StopRequest,
    WindowId,
};
use waldo_screencopy::{Capture, CaptureError, ScreenCapturer};
use waldo_service::ControllerService;
use waldo_socket::{ControllerClient, read_message, write_message};

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct FixedInspector;

impl SessionInspector for FixedInspector {
    fn inspect(&self, include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        let mut snapshot = NiriSnapshot::unavailable("test session");
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
        snapshot.focused_window = Some(WindowId("41".into()));
        if include_windows {
            snapshot.windows = vec![waldo_protocol::WindowState {
                id: WindowId("41".into()),
                title: Some("test window".into()),
                application_id: Some("waldo-test".into()),
                pid: None,
                logical_bounds: None,
                is_floating: true,
                capture: waldo_protocol::WindowCaptureCapability {
                    available: true,
                    limitation: None,
                },
                focused: true,
                workspace_id: None,
                output_id: Some(OutputId("DP-1".into())),
            }];
        }
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

struct ChangedAfterObservationInspector {
    calls: AtomicUsize,
}

#[derive(Clone)]
struct MutableInspector {
    snapshot: Arc<Mutex<NiriSnapshot>>,
}

impl SessionInspector for MutableInspector {
    fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        Ok(self.snapshot.lock().unwrap().clone())
    }
}

impl SessionInspector for ChangedAfterObservationInspector {
    fn inspect(&self, include_windows: bool) -> Result<NiriSnapshot, waldo_niri::InspectionError> {
        let mut snapshot = FixedInspector.inspect(include_windows)?;
        if self.calls.fetch_add(1, Ordering::SeqCst) >= 2 {
            snapshot.outputs[0].logical_bounds.origin.x = 100.0;
        }
        Ok(snapshot)
    }
}

struct FailingFinalCapturer {
    calls: AtomicUsize,
}

struct SettlingFrameCapturer {
    calls: AtomicUsize,
}

struct ContinuationCapturer {
    calls: AtomicUsize,
}

struct CountingCapturer {
    calls: Arc<AtomicUsize>,
}

struct RegionRecordingCapturer {
    regions: Arc<Mutex<Vec<RecordedRegion>>>,
}

#[derive(Debug, PartialEq)]
struct RecordedRegion {
    output: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl ScreenCapturer for RegionRecordingCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        panic!("floating-window observation must use a region capture")
    }

    fn capture_region(
        &self,
        output_name: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<Capture, CaptureError> {
        self.regions.lock().unwrap().push(RecordedRegion {
            output: output_name.into(),
            x,
            y,
            width,
            height,
        });
        Ok(Capture {
            png: b"floating-window-marker".to_vec(),
            width: width as u32,
            height: height as u32,
            captured_at_unix_ms: 1,
        })
    }
}

impl ScreenCapturer for CountingCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        FixedCapturer.capture_output("fixture")
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

impl ScreenCapturer for ContinuationCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Capture {
            png: if call == 0 {
                b"seeded-frame-a".to_vec()
            } else {
                b"seeded-frame-b".to_vec()
            },
            width: 1920,
            height: 1080,
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

impl ScreenCapturer for SettlingFrameCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Capture {
            png: match call {
                0 => b"before-action".to_vec(),
                1..=3 => b"transient-frame".to_vec(),
                _ => b"settled-frame".to_vec(),
            },
            width: 1920,
            height: 1080,
            captured_at_unix_ms: call as u64,
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

impl ScreenCapturer for FailingFinalCapturer {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, output_name: &str) -> Result<Capture, CaptureError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 2 {
            Err(CaptureError::CompositorFailed)
        } else {
            FixedCapturer.capture_output(output_name)
        }
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

#[derive(Clone, Copy)]
struct SuccessfulExecutor;

struct ReleaseTrackingExecutor {
    releases: Arc<AtomicUsize>,
}

struct RecordingPointerExecutor {
    actions: Arc<Mutex<Vec<Action>>>,
}

impl ActionExecutor for SuccessfulExecutor {
    fn execute(
        &self,
        _action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        Ok(ActionRoute::Screencopy.into())
    }
}

impl ActionExecutor for ReleaseTrackingExecutor {
    fn execute(
        &self,
        _action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        Ok(ActionRoute::Screencopy.into())
    }

    fn release_all(&self) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

impl ActionExecutor for RecordingPointerExecutor {
    fn execute(
        &self,
        action: &Action,
        _cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.actions.lock().unwrap().push(action.clone());
        Ok(ActionRoute::VirtualPointer.into())
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::VirtualPointer
    }
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
            message: "cancelled by test Stop request".into(),
            retryable: true,
        }))
    }
}

fn bind_test_service(path: &PathBuf) -> ControllerService {
    ControllerService::bind_with_inspector(
        path,
        Duration::from_secs(30),
        Arc::new(UnavailableInspector::new("test session")),
    )
    .unwrap()
}

fn test_socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "waldo-service-test-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
        .join("controller.sock")
}

async fn connect_and_start_capture(path: &PathBuf, client_name: &str) -> tokio::net::UnixStream {
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    write_message(
        &mut stream,
        &ClientMessage::Hello {
            request_id: RequestId::new(1),
            schema_major: SCHEMA_MAJOR_VERSION,
            client_id: ClientId::new(client_name).unwrap(),
        },
    )
    .await
    .unwrap();
    let _: ServerMessage = read_message(&mut stream).await.unwrap();
    write_message(
        &mut stream,
        &ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        },
    )
    .await
    .unwrap();
    let observed: ServerMessage = read_message(&mut stream).await.unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    write_message(
        &mut stream,
        &ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![Action::KeyChord {
                    keys: vec!["test".into()],
                }],
                verification: Vec::new(),
            },
        },
    )
    .await
    .unwrap();
    stream
}

#[tokio::test]
async fn private_socket_accepts_a_query_from_the_current_user() {
    let path = test_socket();
    let service = bind_test_service(&path);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));

    let mut client = ControllerClient::connect(&path, ClientId::new("test-cli").unwrap())
        .await
        .unwrap();
    let response = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(2),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();

    assert!(matches!(response, ServerMessage::Queried { .. }));
    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!path.exists());
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn continued_observation_reports_real_frame_continuity_and_rejects_other_sources() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(ContinuationCapturer {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("continuation-test").unwrap())
        .await
        .unwrap();
    let source = SourceSelector::Output {
        output_id: OutputId("DP-1".into()),
    };
    let first = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: source.clone(),
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed {
        observation: first, ..
    } = first
    else {
        panic!("initial observation failed")
    };

    let continued = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(3),
            request: ObserveRequest {
                source: source.clone(),
                continue_from: Some(first.id),
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed {
        observation: continued,
        ..
    } = continued
    else {
        panic!("continued observation failed")
    };

    assert_eq!(
        BASE64.decode(&first.image.data_base64).unwrap(),
        b"seeded-frame-a"
    );
    assert_eq!(
        BASE64.decode(&continued.image.data_base64).unwrap(),
        b"seeded-frame-b"
    );
    let continuity = continued.continuation.unwrap();
    assert_eq!(continuity.from, first.id);
    assert!(continuity.frame_changed);
    assert!(!continuity.geometry_changed);
    assert!(!continuity.focus_changed);

    let incompatible = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(4),
            request: ObserveRequest {
                source: SourceSelector::Region {
                    output_id: OutputId("DP-1".into()),
                    region: LogicalRect {
                        origin: LogicalPoint { x: 0.0, y: 0.0 },
                        width: 100.0,
                        height: 100.0,
                    },
                },
                continue_from: Some(first.id),
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        incompatible,
        ServerMessage::Error {
            error: ProtocolError {
                code: ErrorCode::InvalidRequest,
                ..
            },
            ..
        }
    ));

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn tiled_window_limit_is_queryable_and_refused_before_screencopy() {
    let path = test_socket();
    let mut snapshot = FixedInspector.inspect(true).unwrap();
    snapshot.windows[0].logical_bounds = Some(LogicalRect {
        origin: LogicalPoint { x: 100.0, y: 100.0 },
        width: 800.0,
        height: 600.0,
    });
    snapshot.windows[0].is_floating = false;
    snapshot.windows[0].capture = waldo_protocol::WindowCaptureCapability {
        available: false,
        limitation: Some(
            "tiled-window capture is unavailable; observe the containing output instead".into(),
        ),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(MutableInspector {
            snapshot: Arc::new(Mutex::new(snapshot)),
        }),
        Arc::new(CountingCapturer {
            calls: Arc::clone(&calls),
        }),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("tiled-capture-test").unwrap())
        .await
        .unwrap();

    let queried = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(2),
            request: QueryRequest {
                include_windows: true,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = queried else {
        panic!("window capability query failed")
    };
    assert!(!result.windows[0].capture.available);
    assert!(
        result.windows[0]
            .capture
            .limitation
            .as_deref()
            .is_some_and(|limitation| limitation.contains("tiled-window"))
    );

    let refused = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(3),
            request: ObserveRequest {
                source: SourceSelector::Window {
                    window_id: WindowId("41".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        refused,
        ServerMessage::Error {
            error: ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                ..
            },
            ..
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn floating_window_reports_the_exact_output_crop_consumed_by_screencopy() {
    let path = test_socket();
    let mut snapshot = FixedInspector.inspect(true).unwrap();
    let bounds = LogicalRect {
        origin: LogicalPoint { x: 120.0, y: 80.0 },
        width: 800.0,
        height: 600.0,
    };
    snapshot.windows[0].logical_bounds = Some(bounds);
    snapshot.windows[0].is_floating = true;
    snapshot.windows[0].workspace_id = Some(waldo_protocol::WorkspaceId::new(1));
    snapshot.windows[0].capture = waldo_protocol::WindowCaptureCapability {
        available: true,
        limitation: Some("foreground output crop".into()),
    };
    snapshot.workspaces = vec![waldo_protocol::WorkspaceState {
        id: waldo_protocol::WorkspaceId::new(1),
        index: 1,
        name: None,
        output_id: Some(OutputId("DP-1".into())),
        active: true,
        focused: true,
    }];
    let regions = Arc::new(Mutex::new(Vec::new()));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(MutableInspector {
            snapshot: Arc::new(Mutex::new(snapshot)),
        }),
        Arc::new(RegionRecordingCapturer {
            regions: Arc::clone(&regions),
        }),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client =
        ControllerClient::connect(&path, ClientId::new("floating-capture-test").unwrap())
            .await
            .unwrap();

    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Window {
                    window_id: WindowId("41".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("floating-window observation failed")
    };
    assert_eq!(
        BASE64.decode(&observation.image.data_base64).unwrap(),
        b"floating-window-marker"
    );
    assert_eq!(
        observation.capture.route,
        waldo_protocol::CaptureRoute::VisibleWindowCrop
    );
    assert_eq!(observation.capture.output_local_bounds, Some(bounds));
    assert_eq!(observation.coordinates.logical_bounds, bounds);
    assert_eq!(
        observation.coordinates.source_pixels,
        waldo_protocol::PixelSize {
            width: 800,
            height: 600,
        }
    );
    assert_eq!(
        regions.lock().unwrap().as_slice(),
        &[RecordedRegion {
            output: "DP-1".into(),
            x: 120,
            y: 80,
            width: 800,
            height: 600,
        }]
    );

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn topology_mutation_refreshes_query_observe_grounding_and_final_dispatch() {
    let path = test_socket();
    let snapshot = Arc::new(Mutex::new(FixedInspector.inspect(false).unwrap()));
    let inspector = MutableInspector {
        snapshot: Arc::clone(&snapshot),
    };
    let recorded_actions = Arc::new(Mutex::new(Vec::new()));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(inspector),
        Arc::new(ContinuationCapturer {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(RecordingPointerExecutor {
            actions: Arc::clone(&recorded_actions),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("topology-test").unwrap())
        .await
        .unwrap();

    let before = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed {
        observation: before,
        ..
    } = before
    else {
        panic!("initial topology observation failed")
    };

    {
        let mut topology = snapshot.lock().unwrap();
        topology.outputs[0].id = OutputId("DP-2".into());
        topology.outputs[0].name = "DP-2".into();
        topology.outputs[0].logical_bounds.origin.x = 1920.0;
    }

    let queried = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(3),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = queried else {
        panic!("topology query failed")
    };
    assert_eq!(result.outputs[0].id, OutputId("DP-2".into()));

    let stale = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(4),
            request: waldo_protocol::ActRequest {
                observation_id: before.id,
                actions: vec![Action::PointerMoveAbsolute {
                    position: LogicalPoint { x: 100.0, y: 100.0 },
                }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        stale,
        ServerMessage::Error {
            error: ProtocolError {
                code: ErrorCode::TargetLost,
                ..
            },
            ..
        }
    ));
    assert!(recorded_actions.lock().unwrap().is_empty());

    let refreshed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(5),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-2".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed {
        observation: refreshed,
        ..
    } = refreshed
    else {
        panic!("refreshed topology observation failed")
    };
    let target = LogicalPoint {
        x: 2220.0,
        y: 300.0,
    };
    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(6),
            request: waldo_protocol::ActRequest {
                observation_id: refreshed.id,
                actions: vec![Action::PointerMoveAbsolute { position: target }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(acted, ServerMessage::Acted { .. }));
    assert_eq!(
        recorded_actions.lock().unwrap().as_slice(),
        &[Action::PointerMoveAbsolute { position: target }]
    );

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn cross_output_drag_refusal_reaches_no_input_consumer_and_leaves_no_lease() {
    let path = test_socket();
    let mut topology = FixedInspector.inspect(false).unwrap();
    let mut second = topology.outputs[0].clone();
    second.id = OutputId("DP-2".into());
    second.name = "DP-2".into();
    second.logical_bounds.origin.x = 1920.0;
    topology.outputs.push(second);
    let inspector = MutableInspector {
        snapshot: Arc::new(Mutex::new(topology)),
    };
    let recorded_actions = Arc::new(Mutex::new(Vec::new()));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(inspector),
        Arc::new(FixedCapturer),
        Arc::new(RecordingPointerExecutor {
            actions: Arc::clone(&recorded_actions),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("cross-output-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("cross-output test observation failed")
    };
    let refused = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![Action::PointerDrag {
                    from: LogicalPoint {
                        x: 1800.0,
                        y: 500.0,
                    },
                    to: LogicalPoint {
                        x: 2100.0,
                        y: 500.0,
                    },
                    button: waldo_protocol::PointerButton::Primary,
                    duration_ms: 250,
                }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Error { error, .. } = refused else {
        panic!("cross-output drag was not refused")
    };
    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
    assert!(
        error
            .message
            .contains("before pointer movement or button press")
    );
    assert!(recorded_actions.lock().unwrap().is_empty());

    let queried = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(4),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = queried else {
        panic!("post-refusal query failed")
    };
    assert_eq!(result.driving_session, None);
    assert_eq!(result.driving_connection, None);

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn actions_against_unknown_observations_fail_stale() {
    let path = test_socket();
    let service = bind_test_service(&path);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("test-cli").unwrap())
        .await
        .unwrap();

    let response = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(2),
            request: waldo_protocol::ActRequest {
                observation_id: waldo_protocol::ObservationId::new(1),
                actions: Vec::new(),
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();

    assert!(matches!(
        response,
        ServerMessage::Error {
            error: waldo_protocol::ProtocolError {
                code: ErrorCode::StaleObservation,
                ..
            },
            ..
        }
    ));
    let response = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(3),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = response else {
        panic!("controller did not return lease state")
    };
    assert_eq!(result.driving_session, None);
    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn pipelined_act_then_stop_never_executes_after_stop_completion() {
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
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    write_message(
        &mut stream,
        &ClientMessage::Hello {
            request_id: RequestId::new(1),
            schema_major: SCHEMA_MAJOR_VERSION,
            client_id: ClientId::new("cancellation-test").unwrap(),
        },
    )
    .await
    .unwrap();
    let _: ServerMessage = read_message(&mut stream).await.unwrap();
    write_message(
        &mut stream,
        &ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        },
    )
    .await
    .unwrap();
    let observed: ServerMessage = read_message(&mut stream).await.unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    write_message(
        &mut stream,
        &ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![Action::Capture {
                    source: observation.source,
                }],
                verification: Vec::new(),
            },
        },
    )
    .await
    .unwrap();
    write_message(
        &mut stream,
        &ClientMessage::Stop {
            request_id: RequestId::new(4),
            request: StopRequest::default(),
        },
    )
    .await
    .unwrap();
    let first: ServerMessage = read_message(&mut stream).await.unwrap();
    let second: ServerMessage = read_message(&mut stream).await.unwrap();
    let responses = [first, second];
    assert!(
        responses
            .iter()
            .any(|response| matches!(response, ServerMessage::Stopped { .. }))
    );
    let acted = responses
        .into_iter()
        .find_map(|response| match response {
            ServerMessage::Acted { result, .. } => Some(result),
            _ => None,
        })
        .expect("Act must return its cancellation receipts");
    assert_eq!(acted.stop_reason, StopReason::Cancelled);
    let backend_started = started_rx.try_recv().is_ok();
    assert!(!backend_started || finished.load(Ordering::Acquire));
    assert_eq!(
        acted.observation.as_ref().unwrap().stable_output_id.0,
        "DP-1"
    );

    drop(stream);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn stop_response_waits_for_an_active_executor_to_quiesce() {
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
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut stream = connect_and_start_capture(&path, "stop-quiescence-test").await;
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();

    write_message(
        &mut stream,
        &ClientMessage::Stop {
            request_id: RequestId::new(4),
            request: StopRequest::default(),
        },
    )
    .await
    .unwrap();
    let first: ServerMessage = read_message(&mut stream).await.unwrap();
    let second: ServerMessage = read_message(&mut stream).await.unwrap();
    assert!(
        [first, second]
            .iter()
            .any(|response| matches!(response, ServerMessage::Stopped { .. }))
    );
    assert!(finished.load(Ordering::Acquire));

    drop(stream);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn service_shutdown_waits_for_blocking_executor_quiescence() {
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
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let stream = connect_and_start_capture(&path, "shutdown-test").await;
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("service shutdown did not quiesce its executor")
        .unwrap()
        .unwrap();
    assert!(finished.load(Ordering::Acquire));

    drop(stream);
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn service_shutdown_does_not_wait_for_an_idle_pre_handshake_peer() {
    let path = test_socket();
    let service = bind_test_service(&path);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("pre-handshake peer prevented service shutdown")
        .unwrap()
        .unwrap();

    drop(stream);
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn abnormal_client_disconnect_waits_for_executor_quiescence() {
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
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut stream = connect_and_start_capture(&path, "disconnect-test").await;
    tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();

    stream.write_u32(u32::MAX).await.unwrap();
    drop(stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !finished.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("disconnect did not quiesce its executor");

    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn wait_and_capture_actions_return_predicate_and_selected_frame_evidence() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("wait-capture-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    let region = LogicalRect {
        origin: LogicalPoint { x: 10.0, y: 20.0 },
        width: 100.0,
        height: 80.0,
    };
    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![
                    Action::Wait {
                        predicate: Predicate::ProcessExited { pid: u32::MAX },
                        timeout_ms: 0,
                    },
                    Action::Capture {
                        source: SourceSelector::Region {
                            output_id: OutputId("DP-1".into()),
                            region,
                        },
                    },
                ],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = acted else {
        panic!("wait/capture batch did not return an Act result")
    };

    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.receipts[0].route, Some(ActionRoute::ControllerWait));
    assert_eq!(result.receipts[0].predicate_result, Some(true));
    assert_eq!(result.receipts[1].route, Some(ActionRoute::Screencopy));
    assert_eq!(result.receipts[1].predicate_result, None);
    assert_eq!(
        result.observation.unwrap().source,
        SourceSelector::Region {
            output_id: OutputId("DP-1".into()),
            region,
        }
    );

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn false_wait_stops_the_batch_with_a_false_predicate_receipt() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("failed-wait-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![Action::Wait {
                    predicate: Predicate::ProcessExited {
                        pid: std::process::id(),
                    },
                    timeout_ms: 0,
                }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = acted else {
        panic!("failed wait did not return an Act result")
    };

    assert_eq!(result.stop_reason, StopReason::WaitFailed);
    assert_eq!(result.receipts[0].predicate_result, Some(false));
    assert_eq!(
        result.receipts[0].error.as_ref().unwrap().code,
        ErrorCode::WaitFailed
    );

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn final_capture_failure_preserves_receipts_for_executed_actions() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FailingFinalCapturer {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("receipt-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![Action::Capture {
                    source: observation.source,
                }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = acted else {
        panic!("Act did not preserve its result")
    };

    assert_eq!(
        result.receipts[0].status,
        waldo_protocol::ReceiptStatus::Executed
    );
    assert_eq!(result.observation, None);
    assert_eq!(
        result.completion_error.as_ref().unwrap().code,
        ErrorCode::BackendFailure
    );
    assert_eq!(result.stop_reason, StopReason::ExecutionFailed);

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn service_queries_enforce_the_configured_idle_lease_expiry() {
    let path = test_socket();
    let releases = Arc::new(AtomicUsize::new(0));
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::ZERO,
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(ReleaseTrackingExecutor {
            releases: Arc::clone(&releases),
        }),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("expiry-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: Vec::new(),
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(acted, ServerMessage::Acted { .. }));

    let queried = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(4),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = queried else {
        panic!("controller did not return lease state")
    };
    assert_eq!(result.driving_session, None);
    assert!(
        releases.load(Ordering::SeqCst) >= 1,
        "idle lease expiry must release backend-held input"
    );

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn act_returns_mechanical_post_action_verification_results() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(FixedCapturer),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("verification-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };

    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: Vec::new(),
                verification: vec![Predicate::FocusedWindow {
                    window_id: WindowId("41".into()),
                }],
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = acted else {
        panic!("verified action did not return an Act result")
    };
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.verification, vec![true]);

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn act_waits_past_transient_changes_for_a_quiet_post_action_frame() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(FixedInspector),
        Arc::new(SettlingFrameCapturer {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("fresh-frame-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    let before = observation.image.data_base64;

    let acted = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: vec![Action::Text {
                    text: "changed".into(),
                    route: waldo_protocol::TextRoutePreference::VirtualKeyboard,
                }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = acted else {
        panic!("action did not return a result")
    };
    let settled = result.observation.unwrap();
    assert_ne!(settled.image.data_base64, before);
    assert!(settled.captured_at_unix_ms >= 4);

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[tokio::test]
async fn failed_grounding_validation_rolls_back_new_driving_authority() {
    let path = test_socket();
    let service = ControllerService::bind_with_all_backends(
        &path,
        Duration::from_secs(30),
        Arc::new(ChangedAfterObservationInspector {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(FixedCapturer),
        Arc::new(SuccessfulExecutor),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("rollback-test").unwrap())
        .await
        .unwrap();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId("DP-1".into()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("test observation failed")
    };
    let rejected = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: waldo_protocol::ActRequest {
                observation_id: observation.id,
                actions: Vec::new(),
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        rejected,
        ServerMessage::Error {
            error: ProtocolError {
                code: ErrorCode::GeometryChanged,
                ..
            },
            ..
        }
    ));

    let queried = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(4),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = queried else {
        panic!("controller did not return lease state")
    };
    assert_eq!(result.driving_session, None);

    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn binding_refuses_a_symlinked_socket_directory() {
    let path = test_socket();
    let root = path.parent().unwrap();
    let real = root.join("real");
    let linked = root.join("linked");
    fs::create_dir_all(&real).unwrap();
    symlink(&real, &linked).unwrap();

    let error = ControllerService::bind(linked.join("controller.sock"), Duration::from_secs(30))
        .err()
        .expect("a symlinked socket directory must be refused");

    assert!(error.to_string().contains("not a real directory"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn binding_never_replaces_an_unrelated_file() {
    let path = test_socket();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&path, "operator data").unwrap();

    let error = ControllerService::bind(&path, Duration::from_secs(30))
        .err()
        .expect("an unrelated file at the socket path must be refused");

    assert!(
        error
            .to_string()
            .contains("is not a socket owned by this user")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "operator data");
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn binding_does_not_change_an_existing_shared_directory() {
    let path = test_socket();
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();

    let error = ControllerService::bind(&path, Duration::from_secs(30))
        .err()
        .expect("an existing shared directory must be refused");

    assert!(error.to_string().contains("accessible by another user"));
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o755
    );
    let _ = fs::remove_dir_all(parent);
}

#[tokio::test]
async fn shutdown_does_not_remove_a_path_that_replaced_the_bound_socket() {
    let path = test_socket();
    let moved = path.with_extension("moved");
    let service = bind_test_service(&path);
    fs::rename(&path, &moved).unwrap();
    fs::write(&path, "replacement").unwrap();

    drop(service);

    assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    let _ = fs::remove_dir_all(path.parent().unwrap());
}
