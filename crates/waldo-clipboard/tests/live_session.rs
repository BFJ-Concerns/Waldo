//! Live clipboard read proofs; a green headless run exercises none of these.
#[path = "support/owner.rs"]
mod owner;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use waldo_action::ActionExecutor;
use waldo_clipboard::{ClipboardExecutor, ClipboardSnapshot};
use waldo_controller::{Controller, ExecutionToken};
use waldo_protocol::{Action, ActionOutput, ErrorCode};

static CLIPBOARD: Mutex<()> = Mutex::new(());
fn live_session() -> bool {
    if std::env::var_os("NIRI_SOCKET").is_none() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("skipped: no live Niri session (NIRI_SOCKET and WAYLAND_DISPLAY needed)");
        return false;
    }
    true
}
fn token() -> ExecutionToken {
    let mut controller = Controller::new(Duration::from_secs(30));
    let connection = controller.connect();
    controller
        .begin_execution(connection, Duration::ZERO)
        .unwrap()
        .cancellation()
        .clone()
}
struct Restore {
    executor: Arc<ClipboardExecutor>,
    snapshot: Option<ClipboardSnapshot>,
}
impl Restore {
    fn new() -> Self {
        let executor = Arc::new(ClipboardExecutor::initialise().unwrap());
        let snapshot = Some(executor.snapshot().unwrap());
        Self { executor, snapshot }
    }
}
impl Drop for Restore {
    fn drop(&mut self) {
        self.executor
            .restore_temporary(self.snapshot.take().unwrap())
            .expect("restore the selection the test borrowed");
    }
}

#[test]
fn an_owner_that_never_closes_its_pipe_hits_the_read_deadline() {
    if !live_session() {
        return;
    }
    let _serial = CLIPBOARD.lock().unwrap();
    let restore = Restore::new();
    let owner = owner::Owner::start(None);
    let executor = Arc::clone(&restore.executor);
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    let worker = thread::spawn(move || {
        let _ = tx.send(executor.execute(
            &Action::ClipboardRead {
                mime_type: owner::MIME.into(),
            },
            &token(),
        ));
    });
    owner
        .transfers
        .recv_timeout(Duration::from_secs(2))
        .expect("the compositor must request the owner's transfer");
    let result = rx.recv_timeout(Duration::from_secs(6));
    let elapsed = started.elapsed();
    drop(owner); // Also releases the blocked baseline read, so the test can fail.
    worker.join().unwrap();
    let failure = result
        .expect("read must finish even while the owner retains its descriptor")
        .unwrap_err();
    assert_eq!(failure.error().code, ErrorCode::BackendFailure);
    assert!(failure.error().retryable);
    assert!(
        failure.error().message.contains("snapshot limit"),
        "{}",
        failure.error().message
    );
    assert!(
        elapsed < Duration::from_millis(5500),
        "deadline read took {elapsed:?}"
    );
    eprintln!(
        "stalled-owner read returned {:?} after {elapsed:?}",
        failure.error().code
    );
}

#[test]
fn cancelling_a_read_waiting_on_an_owner_returns_promptly() {
    if !live_session() {
        return;
    }
    let _serial = CLIPBOARD.lock().unwrap();
    let restore = Restore::new();
    let owner = owner::Owner::start(None);
    let executor = Arc::clone(&restore.executor);
    let cancellation = token();
    let reading = cancellation.clone();
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = tx.send(executor.execute(
            &Action::ClipboardRead {
                mime_type: owner::MIME.into(),
            },
            &reading,
        ));
    });
    owner
        .transfers
        .recv_timeout(Duration::from_secs(2))
        .expect("cancel during the actual clipboard transfer");
    let started = Instant::now();
    cancellation.cancel();
    let result = rx.recv_timeout(Duration::from_secs(1));
    let elapsed = started.elapsed();
    drop(owner);
    worker.join().unwrap();
    let failure = result
        .expect("cancelled read must promptly stop waiting for its owner")
        .unwrap_err();
    assert_eq!(failure.error().code, ErrorCode::Cancelled);
    assert!(
        elapsed < Duration::from_millis(500),
        "cancelled read took {elapsed:?}"
    );
    eprintln!(
        "cancelled owner read returned {:?} after {elapsed:?}",
        failure.error().code
    );
}

#[test]
fn a_completed_read_returns_the_owners_exact_bytes() {
    if !live_session() {
        return;
    }
    let _serial = CLIPBOARD.lock().unwrap();
    let restore = Restore::new();
    let expected = b"complete\0representation";
    let owner = owner::Owner::start(Some(expected.to_vec()));
    let result = restore
        .executor
        .execute(
            &Action::ClipboardRead {
                mime_type: owner::MIME.into(),
            },
            &token(),
        )
        .unwrap();
    let Some(ActionOutput::Clipboard {
        mime_type,
        data_base64,
    }) = result.output
    else {
        panic!("read must return clipboard bytes");
    };
    assert_eq!(mime_type, owner::MIME);
    assert_eq!(BASE64.decode(data_base64).unwrap(), expected);
    drop(owner);
}
