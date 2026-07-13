use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use tokio::sync::oneshot;
use waldo_protocol::{
    ClientId, ClientMessage, ObserveRequest, OutputId, RequestId, ServerMessage, SourceSelector,
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
async fn observe_returns_a_png_and_grounding_metadata_through_the_service() {
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
    let service = ControllerService::bind(&path, Duration::from_secs(30)).unwrap();
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
    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}
