use std::{
    fs,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use waldo_protocol::ClientId;
use waldo_socket::ControllerClient;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("waldod did not create {}", path.display());
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("waldod did not exit after SIGTERM");
}

#[tokio::test]
async fn sigterm_removes_the_bound_controller_socket() {
    let root = std::env::temp_dir().join(format!("waldo-daemon-shutdown-{}", std::process::id()));
    let socket = root.join("waldo/controller.sock");
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    fs::set_permissions(socket.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();

    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_waldod"))
            .args(["--socket", socket.to_str().unwrap()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_socket(&socket, Duration::from_secs(5));
    // A bound socket precedes backend initialisation and signal registration.
    // A completed handshake proves the daemon is serving before we stop it.
    let client = tokio::time::timeout(
        Duration::from_secs(5),
        ControllerClient::connect(&socket, ClientId::new("shutdown-test").unwrap()),
    )
    .await
    .expect("waldod did not serve its socket")
    .expect("waldod handshake failed");
    drop(client);

    let signal_result =
        unsafe { nix::libc::kill(i32::try_from(child.0.id()).unwrap(), nix::libc::SIGTERM) };
    assert_eq!(signal_result, 0);
    let status = wait_for_exit(&mut child.0, Duration::from_secs(5));

    assert!(status.success(), "{status}");
    assert!(!socket.exists());
    fs::remove_dir(socket.parent().unwrap()).unwrap();
    fs::remove_dir(&root).unwrap();
}
