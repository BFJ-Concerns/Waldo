use std::{
    fs,
    time::{Duration, Instant},
};

use tokio::sync::oneshot;
use waldo_protocol::{
    ActRequest, Action, ActionOutput, ClientId, ClientMessage, ObserveRequest, QueryRequest,
    ReceiptStatus, RequestId, ServerMessage, SourceSelector, StopRequest,
};
use waldo_service::ControllerService;
use waldo_socket::ControllerClient;

#[tokio::test]
#[ignore = "requires a live Niri Wayland session"]
async fn empty_clipboard_preservation_returns_through_the_service() {
    let root = std::env::temp_dir().join(format!("waldo-live-clipboard-{}", std::process::id()));
    let socket = root.join("controller.sock");
    let service = ControllerService::bind(
        &socket,
        Duration::from_secs(30),
        &waldo_settings::Settings::from_sources(None, [], None).unwrap(),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(service.run_until(async {
        let _ = shutdown_rx.await;
    }));
    let mut client = ControllerClient::connect(
        &socket,
        ClientId::new("live-clipboard-bound-probe").unwrap(),
    )
    .await
    .unwrap();
    let queried = client
        .request(&ClientMessage::Query {
            request_id: RequestId::new(2),
            request: QueryRequest::default(),
        })
        .await
        .unwrap();
    let ServerMessage::Queried { result, .. } = queried else {
        panic!("live Query failed")
    };
    let output = result
        .outputs
        .iter()
        .find(|output| output.focused)
        .expect("Niri has no focused output")
        .id
        .clone();
    let observed = client
        .request(&ClientMessage::Observe {
            request_id: RequestId::new(3),
            request: ObserveRequest {
                source: SourceSelector::Output { output_id: output },
                continue_from: None,
                include_accessibility: false,
            },
        })
        .await
        .unwrap();
    let ServerMessage::Observed { observation, .. } = observed else {
        panic!("live Observe failed")
    };

    let started = Instant::now();
    let preserved = client
        .request(&ClientMessage::Act {
            request_id: RequestId::new(4),
            request: ActRequest {
                observation_id: observation.id,
                actions: vec![Action::ClipboardPreserve],
                verification: Vec::new(),
            },
        })
        .await
        .unwrap();
    let ServerMessage::Acted { result, .. } = preserved else {
        panic!("ClipboardPreserve failed: {preserved:?}")
    };
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(result.receipts[0].status, ReceiptStatus::Executed);
    assert!(matches!(
        result.receipts[0].output,
        Some(ActionOutput::ClipboardPreservation { .. })
    ));

    let stopped = client
        .request(&ClientMessage::Stop {
            request_id: RequestId::new(5),
            request: StopRequest::default(),
        })
        .await
        .unwrap();
    assert!(matches!(stopped, ServerMessage::Stopped { .. }));
    drop(client);
    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
    let _ = fs::remove_dir_all(root);
}
