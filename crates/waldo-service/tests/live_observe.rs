use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tokio::sync::oneshot;
use waldo_protocol::{
    ActRequest, Action, ClientId, ClientMessage, ObserveRequest, OutputId, ReceiptStatus,
    RequestId, ServerMessage, SourceSelector, StopReason, WindowId, WindowOperation,
};
use waldo_service::ControllerService;
use waldo_socket::ControllerClient;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn test_socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "waldo-live-observe-test-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
        .join("controller.sock")
}

#[tokio::test]
async fn observation_grounding_reaches_capture_and_fail_closed_target_checks() {
    if std::env::var_os("NIRI_SOCKET").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    let focused = niri_ipc::socket::Socket::connect()
        .unwrap()
        .send(niri_ipc::Request::FocusedOutput)
        .unwrap()
        .unwrap();
    let niri_ipc::Response::FocusedOutput(Some(output)) = focused else {
        panic!("Niri has no focused output");
    };
    let path = test_socket();
    let service = ControllerService::bind(
        &path,
        Duration::from_secs(30),
        &waldo_settings::Settings::from_sources(None, [], None).unwrap(),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(&path, ClientId::new("observe-test").unwrap())
        .await
        .unwrap();

    let response = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(2),
            request: ObserveRequest {
                source: SourceSelector::Output {
                    output_id: OutputId(output.name.clone()),
                },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();

    let ServerMessage::Observed { observation, .. } = response else {
        panic!("controller did not return an observation");
    };
    let png = BASE64.decode(&observation.image.data_base64).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(observation.stable_output_id.0, output.name);
    assert_eq!(
        observation.coordinates.source_pixels,
        observation.coordinates.model_image_pixels
    );
    assert!(observation.coordinates.logical_bounds.width > 0.0);
    let source = observation.source.clone();
    let observation_id = observation.id;

    let response = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(3),
            request: ActRequest {
                observation_id,
                actions: vec![Action::Capture { source }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = response else {
        panic!("controller did not return an action result");
    };
    assert_eq!(result.stop_reason, StopReason::Completed);
    assert_eq!(result.first_not_executed, None);
    assert_eq!(result.receipts[0].status, ReceiptStatus::Executed);
    assert_eq!(
        result.receipts[0].route,
        Some(waldo_protocol::ActionRoute::Screencopy)
    );
    assert_eq!(result.receipts[0].error, None);
    let resulting_observation = result.observation.as_ref().unwrap();
    assert_eq!(result.completion_error, None);
    assert_ne!(resulting_observation.id, observation_id);
    assert!(
        BASE64
            .decode(&resulting_observation.image.data_base64)
            .unwrap()
            .starts_with(b"\x89PNG\r\n\x1a\n")
    );

    // A deliberately absent target proves that live Niri state is rechecked
    // before dispatch without disturbing any real window in the session.
    let response = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(4),
            request: ActRequest {
                observation_id: resulting_observation.id,
                actions: vec![Action::Window {
                    window_id: WindowId(u64::MAX.to_string()),
                    operation: WindowOperation::Focus,
                }],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Error { error, .. } = response else {
        panic!("a vanished Niri target was not rejected during grounding");
    };
    assert_eq!(error.code, waldo_protocol::ErrorCode::TargetLost);
    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}
