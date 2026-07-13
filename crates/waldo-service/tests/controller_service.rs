use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tokio::sync::oneshot;
use waldo_protocol::{ClientId, ClientMessage, ErrorCode, QueryRequest, RequestId, ServerMessage};
use waldo_service::ControllerService;
use waldo_socket::ControllerClient;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn test_socket() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "waldo-service-test-{}-{}",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
        .join("controller.sock")
}

#[tokio::test]
async fn private_socket_accepts_a_query_from_the_current_user() {
    let path = test_socket();
    let service = ControllerService::bind(&path, Duration::from_secs(30)).unwrap();
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
async fn unavailable_desktop_operations_fail_explicitly() {
    let path = test_socket();
    let service = ControllerService::bind(&path, Duration::from_secs(30)).unwrap();
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
                code: ErrorCode::UnsupportedCapability,
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
    let service = ControllerService::bind(&path, Duration::from_secs(30)).unwrap();
    fs::rename(&path, &moved).unwrap();
    fs::write(&path, "replacement").unwrap();

    drop(service);

    assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
    let _ = fs::remove_dir_all(path.parent().unwrap());
}
