use std::{
    fs,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tokio::sync::oneshot;
use waldo_cli::{Arguments, Command, execute};
use waldo_niri::UnavailableInspector;
use waldo_service::ControllerService;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

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
        client_id: "integration-test".into(),
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
        client_id: "integration-test".into(),
        command: Command::Schema,
    })
    .await
    .unwrap();

    assert_eq!(result["schema_major"], 1);
}
