//! MIME-typed clipboard execution through Wayland data-control.

mod stable_offer;

use std::{collections::HashSet, io::Read, sync::Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use waldo_action::{ActionExecution, ActionExecutionFailure, ActionExecutor};
use waldo_controller::ExecutionToken;
use waldo_protocol::{
    Action, ActionOutput, ActionRoute, ClipboardPreservationId, ClipboardRepresentation,
    ConnectionId, DrivingSessionId, ErrorCode, ProtocolError,
};
use wl_clipboard_rs::{
    copy::{
        ClipboardType as CopyClipboardType, MimeType as CopyMimeType, Options, Seat as CopySeat,
        ServeRequests, Source, clear,
    },
    paste::{
        ClipboardType, Error as PasteError, MimeType as PasteMimeType, Seat, get_contents,
        get_mime_types,
    },
};

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Returns the named Wayland seat used for the regular clipboard.
pub fn regular_clipboard_seat() -> Result<String, ProtocolError> {
    stable_offer::probe()
}

/// Reads and owns the regular Wayland clipboard through data-control.
#[derive(Debug)]
pub struct ClipboardExecutor {
    seat: String,
    preservation: Mutex<PreservationState>,
}

/// Complete bounded clipboard contents retained while a temporary paste is served.
#[derive(Clone, Debug)]
pub struct ClipboardSnapshot {
    seat: String,
    sources: Vec<(String, Vec<u8>)>,
}

#[derive(Debug, Default)]
struct PreservationState {
    next_id: u64,
    guard: Option<PreservationGuard>,
    fallback_restore: Option<ClipboardSnapshot>,
    published_restore: Option<ClipboardSnapshot>,
    publication: Option<stable_offer::Publication>,
    failure: Option<ProtocolError>,
}

#[derive(Debug)]
struct PreservationGuard {
    id: ClipboardPreservationId,
    owner: ConnectionId,
    session: DrivingSessionId,
    snapshot: ClipboardSnapshot,
}

impl ClipboardExecutor {
    /// Probes the seat and data-control protocol before advertising support.
    pub fn initialise() -> Result<Self, ProtocolError> {
        let seat = regular_clipboard_seat()?;
        match get_mime_types(ClipboardType::Regular, Seat::Specific(seat.as_str())) {
            Ok(_) | Err(PasteError::ClipboardEmpty) => Ok(Self {
                seat,
                preservation: Mutex::new(PreservationState::default()),
            }),
            Err(error) => Err(backend_failure(
                format!("initialise Wayland clipboard: {error}"),
                false,
            )),
        }
    }

    /// Reads every advertised MIME representation for later restoration.
    pub fn snapshot(&self) -> Result<ClipboardSnapshot, ProtocolError> {
        self.ensure_mutation_allowed()?;
        let sources = stable_offer::snapshot(&self.seat, MAX_CLIPBOARD_BYTES)?;
        for (mime_type, _) in &sources {
            validate_mime_type(mime_type)?;
        }
        Ok(ClipboardSnapshot {
            seat: self.seat.clone(),
            sources,
        })
    }

    /// Offers temporary UTF-8 text and retires it after one paste request.
    pub fn offer_text_once(&self, text: &str) -> Result<(), ActionExecutionFailure> {
        self.ensure_mutation_allowed()
            .map_err(ActionExecutionFailure::not_executed)?;
        let mut options = Options::new();
        options.seat(CopySeat::Specific(self.seat.clone()));
        options.serve_requests(ServeRequests::Only(1));
        options
            .copy(Source::Bytes(text.as_bytes().into()), CopyMimeType::Text)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    backend_failure(format!("offer temporary paste text: {error}"), true),
                )
            })?;
        self.replace_publication(None, None)
            .map_err(|error| ActionExecutionFailure::indeterminate(ActionRoute::Clipboard, error))
    }

    /// Reports whether the one-shot temporary clipboard has been consumed.
    pub fn is_empty(&self) -> Result<bool, ProtocolError> {
        match get_mime_types(ClipboardType::Regular, Seat::Specific(self.seat.as_str())) {
            Ok(_) => Ok(false),
            Err(PasteError::ClipboardEmpty) => Ok(true),
            Err(error) => Err(backend_failure(
                format!("inspect temporary Wayland clipboard: {error}"),
                true,
            )),
        }
    }

    /// Restores every MIME representation captured before a temporary paste.
    fn restore_snapshot(
        snapshot: &ClipboardSnapshot,
    ) -> Result<Option<stable_offer::Publication>, ProtocolError> {
        if snapshot.sources.is_empty() {
            clear(
                CopyClipboardType::Regular,
                CopySeat::Specific(snapshot.seat.clone()),
            )
            .map_err(|error| {
                backend_failure(format!("restore empty Wayland clipboard: {error}"), true)
            })?;
            return Ok(None);
        }
        stable_offer::publish(&snapshot.seat, snapshot.sources.clone())
            .map(Some)
            .map_err(|error| {
                backend_failure(
                    format!(
                        "restore ordered Wayland clipboard contents: {}",
                        error.message
                    ),
                    error.retryable,
                )
            })
    }

    fn read(&self, mime_type: &str) -> Result<ActionExecution, ActionExecutionFailure> {
        validate_mime_type(mime_type).map_err(ActionExecutionFailure::not_executed)?;
        let (reader, selected_mime) = get_contents(
            ClipboardType::Regular,
            Seat::Specific(self.seat.as_str()),
            PasteMimeType::Specific(mime_type),
        )
        .map_err(|error| {
            ActionExecutionFailure::not_executed(backend_failure(
                format!("read Wayland clipboard as {mime_type}: {error}"),
                true,
            ))
        })?;
        let mut data = Vec::new();
        reader
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut data)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    backend_failure(format!("receive Wayland clipboard data: {error}"), true),
                )
            })?;
        if data.len() > MAX_CLIPBOARD_BYTES {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: format!(
                    "clipboard value exceeds Waldo's {MAX_CLIPBOARD_BYTES}-byte transfer limit"
                ),
                retryable: false,
            }));
        }

        Ok(ActionExecution {
            route: ActionRoute::Clipboard,
            output: Some(ActionOutput::Clipboard {
                mime_type: selected_mime,
                data_base64: BASE64.encode(data),
            }),
        })
    }

    fn write(
        &self,
        mime_type: &str,
        data_base64: &str,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.ensure_mutation_allowed()
            .map_err(ActionExecutionFailure::not_executed)?;
        validate_mime_type(mime_type).map_err(ActionExecutionFailure::not_executed)?;
        let data = BASE64.decode(data_base64).map_err(|error| {
            ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: format!("clipboard data is not valid base64: {error}"),
                retryable: false,
            })
        })?;
        if data.len() > MAX_CLIPBOARD_BYTES {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: format!(
                    "clipboard value exceeds Waldo's {MAX_CLIPBOARD_BYTES}-byte transfer limit"
                ),
                retryable: false,
            }));
        }
        let mut options = Options::new();
        options.seat(CopySeat::Specific(self.seat.clone()));
        options
            .copy(
                Source::Bytes(data.into()),
                CopyMimeType::Specific(mime_type.to_owned()),
            )
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    backend_failure(format!("write Wayland clipboard: {error}"), true),
                )
            })?;
        self.replace_publication(None, None).map_err(|error| {
            ActionExecutionFailure::indeterminate(ActionRoute::Clipboard, error)
        })?;
        Ok(ActionRoute::Clipboard.into())
    }

    fn write_offer(
        &self,
        representations: &[ClipboardRepresentation],
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.ensure_mutation_allowed()
            .map_err(ActionExecutionFailure::not_executed)?;
        let representations = decode_representations(representations, MAX_CLIPBOARD_BYTES)
            .map_err(ActionExecutionFailure::not_executed)?;
        let publication = stable_offer::publish(&self.seat, representations).map_err(|error| {
            ActionExecutionFailure::indeterminate(
                ActionRoute::Clipboard,
                backend_failure(
                    format!(
                        "write ordered atomic Wayland clipboard offer: {}",
                        error.message
                    ),
                    error.retryable,
                ),
            )
        })?;
        self.replace_publication(Some(publication), None)
            .map_err(|error| {
                ActionExecutionFailure::indeterminate(ActionRoute::Clipboard, error)
            })?;
        Ok(ActionRoute::Clipboard.into())
    }

    fn preserve(
        &self,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        {
            let state = self.preservation.lock().map_err(|_| {
                ActionExecutionFailure::not_executed(internal(
                    "clipboard preservation state lock is poisoned",
                ))
            })?;
            if let Some(error) = &state.failure {
                return Err(ActionExecutionFailure::not_executed(blocked_by_restore(
                    error,
                )));
            }
            if state.guard.is_some() {
                return Err(ActionExecutionFailure::not_executed(invalid_request(
                    "this driving session already owns a clipboard preservation guard",
                )));
            }
        }

        let snapshot = self
            .snapshot()
            .map_err(ActionExecutionFailure::not_executed)?;
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        let mut state = self.preservation.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(internal(
                "clipboard preservation state lock is poisoned",
            ))
        })?;
        if state.guard.is_some() {
            return Err(ActionExecutionFailure::not_executed(invalid_request(
                "this driving session already owns a clipboard preservation guard",
            )));
        }
        state.next_id = state.next_id.checked_add(1).ok_or_else(|| {
            ActionExecutionFailure::not_executed(internal(
                "clipboard preservation identifier space exhausted",
            ))
        })?;
        let id = ClipboardPreservationId::new(state.next_id);
        state.guard = Some(PreservationGuard {
            id,
            owner: cancellation.owner(),
            session: cancellation.driving_session(),
            snapshot,
        });
        Ok(ActionExecution {
            route: ActionRoute::Clipboard,
            output: Some(ActionOutput::ClipboardPreservation {
                preservation_id: id,
            }),
        })
    }

    fn restore_guard(
        &self,
        preservation_id: ClipboardPreservationId,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        self.restore_guard_with(preservation_id, cancellation, Self::restore_snapshot)
    }

    fn restore_guard_with(
        &self,
        preservation_id: ClipboardPreservationId,
        cancellation: &ExecutionToken,
        restore: impl FnOnce(
            &ClipboardSnapshot,
        ) -> Result<Option<stable_offer::Publication>, ProtocolError>,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        let mut state = self.preservation.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(internal(
                "clipboard preservation state lock is poisoned",
            ))
        })?;
        let guard = state.guard.as_ref().ok_or_else(|| {
            ActionExecutionFailure::not_executed(invalid_request(
                "no clipboard preservation guard is active",
            ))
        })?;
        if guard.id != preservation_id
            || guard.owner != cancellation.owner()
            || guard.session != cancellation.driving_session()
        {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::NotLeaseOwner,
                message: "clipboard preservation guard belongs to another driving session".into(),
                retryable: false,
            }));
        }
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        let snapshot = guard.snapshot.clone();
        let result = restore(&snapshot);
        match result {
            Ok(publication) => {
                state.guard = None;
                state.fallback_restore = None;
                state.published_restore = publication.as_ref().map(|_| snapshot);
                state.publication = publication;
                state.failure = None;
                Ok(ActionRoute::Clipboard.into())
            }
            Err(error) => {
                state.failure = Some(error.clone());
                Err(ActionExecutionFailure::indeterminate(
                    ActionRoute::Clipboard,
                    error,
                ))
            }
        }
    }

    fn keep_guard(
        &self,
        preservation_id: ClipboardPreservationId,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        let mut state = self.preservation.lock().map_err(|_| {
            ActionExecutionFailure::not_executed(internal(
                "clipboard preservation state lock is poisoned",
            ))
        })?;
        Self::promote_publication_failure(&mut state);
        let guard = state.guard.as_ref().ok_or_else(|| {
            ActionExecutionFailure::not_executed(invalid_request(
                "no clipboard preservation guard is active",
            ))
        })?;
        if guard.id != preservation_id
            || guard.owner != cancellation.owner()
            || guard.session != cancellation.driving_session()
        {
            return Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::NotLeaseOwner,
                message: "clipboard preservation guard belongs to another driving session".into(),
                retryable: false,
            }));
        }
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        if let Some(error) = &state.failure {
            return Err(ActionExecutionFailure::not_executed(blocked_by_restore(
                error,
            )));
        }
        state.guard = None;
        Ok(ActionRoute::Clipboard.into())
    }

    /// Restores a temporary-paste snapshot and retains it after uncertain failure.
    pub fn restore_temporary(&self, snapshot: ClipboardSnapshot) -> Result<(), ProtocolError> {
        match Self::restore_snapshot(&snapshot) {
            Ok(publication) => {
                self.replace_publication(
                    publication.clone(),
                    publication.as_ref().map(|_| snapshot),
                )?;
                Ok(())
            }
            Err(error) => {
                if let Ok(mut state) = self.preservation.lock() {
                    if state.guard.is_none() {
                        state.fallback_restore = Some(snapshot);
                    }
                    state.failure = Some(error.clone());
                }
                Err(error)
            }
        }
    }

    /// Attempts restoration at every driving-session cleanup boundary, and
    /// returns the restoration failure the clipboard is still blocked on.
    pub fn cleanup_preservation(&self) -> Result<(), ProtocolError> {
        self.cleanup_with(Self::restore_snapshot)
    }

    fn cleanup_with(
        &self,
        restore: impl FnOnce(
            &ClipboardSnapshot,
        ) -> Result<Option<stable_offer::Publication>, ProtocolError>,
    ) -> Result<(), ProtocolError> {
        let mut state = self
            .preservation
            .lock()
            .map_err(|_| internal("clipboard preservation state lock is poisoned"))?;
        Self::promote_publication_failure(&mut state);
        let snapshot = state
            .guard
            .as_ref()
            .map(|guard| &guard.snapshot)
            .or(state.fallback_restore.as_ref());
        let Some(snapshot) = snapshot.cloned() else {
            return Ok(());
        };
        match restore(&snapshot) {
            Ok(publication) => {
                state.guard = None;
                state.fallback_restore = None;
                state.published_restore = publication.as_ref().map(|_| snapshot);
                state.publication = publication;
                state.failure = None;
                Ok(())
            }
            Err(error) => {
                state.failure = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Returns a payload-free marker for an unresolved restoration failure.
    pub fn limitation(&self) -> Option<String> {
        let mut state = self.preservation.lock().ok()?;
        Self::promote_publication_failure(&mut state);
        state.failure.as_ref().map(restoration_limitation)
    }

    fn ensure_mutation_allowed(&self) -> Result<(), ProtocolError> {
        let mut state = self
            .preservation
            .lock()
            .map_err(|_| internal("clipboard preservation state lock is poisoned"))?;
        Self::promote_publication_failure(&mut state);
        match &state.failure {
            Some(error) => Err(blocked_by_restore(error)),
            None => Ok(()),
        }
    }

    fn replace_publication(
        &self,
        publication: Option<stable_offer::Publication>,
        published_restore: Option<ClipboardSnapshot>,
    ) -> Result<(), ProtocolError> {
        let mut state = self
            .preservation
            .lock()
            .map_err(|_| internal("clipboard preservation state lock is poisoned"))?;
        Self::promote_publication_failure(&mut state);
        if let Some(error) = &state.failure {
            return Err(blocked_by_restore(error));
        }
        state.publication = publication;
        state.published_restore = published_restore;
        Ok(())
    }

    fn promote_publication_failure(state: &mut PreservationState) {
        if state.failure.is_some() {
            return;
        }
        let Some(error) = state
            .publication
            .as_ref()
            .and_then(stable_offer::Publication::failure)
        else {
            return;
        };
        if state.fallback_restore.is_none() {
            state.fallback_restore = state.published_restore.take();
        }
        state.failure = Some(error);
    }
}

impl ActionExecutor for ClipboardExecutor {
    fn execute(
        &self,
        action: &Action,
        cancellation: &ExecutionToken,
    ) -> Result<ActionExecution, ActionExecutionFailure> {
        if cancellation.is_cancelled() {
            return Err(ActionExecutionFailure::not_executed(cancelled()));
        }
        match action {
            Action::ClipboardRead { mime_type } => self.read(mime_type),
            Action::ClipboardWrite {
                mime_type,
                data_base64,
            } => self.write(mime_type, data_base64),
            Action::ClipboardWriteOffer { representations } => self.write_offer(representations),
            Action::ClipboardPreserve => self.preserve(cancellation),
            Action::ClipboardRestore { preservation_id } => {
                self.restore_guard(*preservation_id, cancellation)
            }
            Action::ClipboardKeep { preservation_id } => {
                self.keep_guard(*preservation_id, cancellation)
            }
            _ => Err(ActionExecutionFailure::not_executed(ProtocolError {
                code: ErrorCode::UnsupportedCapability,
                message: "this action has no clipboard route".into(),
                retryable: false,
            })),
        }
    }

    fn supports(&self, route: ActionRoute) -> bool {
        route == ActionRoute::Clipboard
    }

    fn limitation(&self, route: ActionRoute) -> Option<String> {
        (route == ActionRoute::Clipboard)
            .then(|| self.limitation())
            .flatten()
    }

    fn release_all(&self) -> Option<String> {
        self.cleanup_preservation()
            .err()
            .map(|error| restoration_limitation(&error))
    }
}

fn decode_representations(
    representations: &[ClipboardRepresentation],
    max_bytes: usize,
) -> Result<Vec<(String, Vec<u8>)>, ProtocolError> {
    if representations.is_empty() {
        return Err(invalid_request(
            "an atomic clipboard offer requires at least one MIME representation",
        ));
    }
    let mut mime_types = HashSet::with_capacity(representations.len());
    let mut decoded = Vec::with_capacity(representations.len());
    let mut total = 0_usize;
    for representation in representations {
        validate_mime_type(&representation.mime_type)?;
        if !mime_types.insert(representation.mime_type.as_str()) {
            return Err(invalid_request(format!(
                "atomic clipboard offer repeats MIME type {}",
                representation.mime_type
            )));
        }
        let data = BASE64
            .decode(&representation.data_base64)
            .map_err(|error| {
                invalid_request(format!(
                    "clipboard data for {} is not valid base64: {error}",
                    representation.mime_type
                ))
            })?;
        total = total.saturating_add(data.len());
        if total > max_bytes {
            return Err(invalid_request(format!(
                "clipboard representations exceed Waldo's {max_bytes}-byte transfer limit"
            )));
        }
        decoded.push((representation.mime_type.clone(), data));
    }
    Ok(decoded)
}

fn validate_mime_type(mime_type: &str) -> Result<(), ProtocolError> {
    if mime_type.is_empty() || mime_type.contains('\0') {
        return Err(ProtocolError {
            code: ErrorCode::InvalidRequest,
            message: "clipboard MIME type must be non-empty and contain no NUL byte".into(),
            retryable: false,
        });
    }
    Ok(())
}

fn cancelled() -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Cancelled,
        message: "the action batch was cancelled before clipboard dispatch".into(),
        retryable: false,
    }
}

fn backend_failure(message: impl Into<String>, retryable: bool) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::BackendFailure,
        message: message.into(),
        retryable,
    }
}

fn invalid_request(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::InvalidRequest,
        message: message.into(),
        retryable: false,
    }
}

fn internal(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::Internal,
        message: message.into(),
        retryable: true,
    }
}

/// Payload-free marker for a restoration failure the clipboard is blocked on.
fn restoration_limitation(error: &ProtocolError) -> String {
    format!(
        "clipboard mutation is blocked until exact restoration succeeds: {}",
        error.message
    )
}

fn blocked_by_restore(error: &ProtocolError) -> ProtocolError {
    ProtocolError {
        code: ErrorCode::UnsupportedCapability,
        message: format!(
            "clipboard mutation is blocked by an unresolved exact-restoration failure: {}",
            error.message
        ),
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::DirBuilderExt,
        process::{Command, Stdio},
        sync::Mutex,
        thread,
        time::{Duration, Instant},
    };

    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use waldo_action::{ActionExecutionFailure, ActionExecutor};
    use waldo_controller::Controller;
    use waldo_protocol::{
        Action, ClipboardPreservationId, ClipboardRepresentation, ConnectionId, DrivingSessionId,
        ErrorCode,
    };

    use super::{
        ClipboardExecutor, ClipboardSnapshot, PreservationGuard, PreservationState,
        backend_failure, decode_representations, validate_mime_type,
    };

    #[test]
    #[ignore = "requires a live Wayland data-control session"]
    fn live_snapshot_returns_within_its_hard_deadline() {
        let executor = ClipboardExecutor::initialise().unwrap();
        let started = Instant::now();
        let _ = executor.snapshot();
        assert!(started.elapsed() < Duration::from_secs(6));
    }

    #[test]
    #[ignore = "requires the live Niri Wayland session and GTK fixture"]
    fn live_snapshot_is_bounded_with_the_gtk_clipboard_client_present() {
        let directory = std::path::Path::new("/dev/shm")
            .join(format!("waldo-live-stable-offer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let state = directory.join("state.json");
        let control = directory.join("control.json");
        let secret = directory.join("secret");
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../waldo-verification/fixtures/clipboard_text.py");
        let mut child = Command::new("python3")
            .arg(fixture)
            .args(["--state", state.to_str().unwrap()])
            .args(["--control", control.to_str().unwrap()])
            .args(["--secret-handoff", secret.to_str().unwrap()])
            .args(["--seed", "51429"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !state.exists() && Instant::now() < ready_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(state.exists(), "GTK clipboard fixture did not become ready");

        let executor = ClipboardExecutor::initialise().unwrap();
        let started = Instant::now();
        let result = executor.snapshot();
        let elapsed = started.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(directory);

        let _ = result;
        assert!(elapsed < Duration::from_secs(6));
    }

    #[test]
    #[ignore = "requires an empty live Wayland clipboard and temporarily owns it"]
    fn live_ordered_publication_advertises_and_restores_one_exact_generation() {
        let executor = ClipboardExecutor::initialise().unwrap();
        let original = executor.snapshot().unwrap();
        if !original.sources.is_empty() {
            return;
        }
        let expected = vec![
            ("application/x-waldo-first".into(), b"first".to_vec()),
            ("application/x-waldo-second".into(), b"second".to_vec()),
        ];

        let proof = (|| {
            super::stable_offer::publish(&executor.seat, expected.clone())?;
            super::stable_offer::snapshot(&executor.seat, 1024)
        })();
        let cleared = ClipboardExecutor::restore_snapshot(&original);

        cleared.expect("clear the test offer after the live ordered-offer proof");
        assert_eq!(proof.unwrap(), expected);
    }

    #[test]
    fn mime_types_fail_closed_before_wayland_dispatch() {
        assert_eq!(
            validate_mime_type("").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            validate_mime_type("text/plain\0hidden").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert!(validate_mime_type("text/plain;charset=utf-8").is_ok());
    }

    #[test]
    fn atomic_offer_validation_preserves_order_and_exact_bytes() {
        let representations = [
            representation("text/html", b"<b>html</b>"),
            representation("text/plain", b"plain"),
        ];

        assert_eq!(
            decode_representations(&representations, 1024).unwrap(),
            [
                ("text/html".into(), b"<b>html</b>".to_vec()),
                ("text/plain".into(), b"plain".to_vec()),
            ]
        );
    }

    #[test]
    fn atomic_offer_validation_rejects_the_complete_invalid_set_before_dispatch() {
        assert_eq!(
            decode_representations(&[], 1024).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            decode_representations(
                &[
                    representation("text/plain", b"one"),
                    representation("text/plain", b"two"),
                ],
                1024,
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            decode_representations(
                &[
                    representation("text/plain", b"one"),
                    ClipboardRepresentation {
                        mime_type: "text/html".into(),
                        data_base64: "not base64".into(),
                    },
                ],
                1024,
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            decode_representations(
                &[
                    representation("text/html", b"1234"),
                    representation("text/plain", b"56"),
                ],
                5,
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn cleanup_failure_retains_the_guard_blocks_mutation_and_surfaces_a_limitation() {
        let executor = guarded_executor(ConnectionId::new(1), DrivingSessionId::new(1));
        let failure = executor
            .cleanup_with(|_| {
                Err(backend_failure(
                    "deliberate restoration acknowledgement loss",
                    true,
                ))
            })
            .unwrap_err();
        assert!(failure.message.contains("acknowledgement loss"));

        assert!(executor.ensure_mutation_allowed().is_err());
        let limitation = executor.limitation().unwrap();
        assert!(limitation.contains("exact restoration"));
        assert!(!limitation.contains("payload"));
        assert!(executor.preservation.lock().unwrap().guard.is_some());

        executor.cleanup_with(|_| Ok(None)).unwrap();
        assert!(executor.ensure_mutation_allowed().is_ok());
        assert!(executor.limitation().is_none());
        assert!(executor.preservation.lock().unwrap().guard.is_none());
    }

    #[test]
    fn background_publication_failure_promotes_the_private_restore_fallback() {
        let executor = guarded_executor(ConnectionId::new(1), DrivingSessionId::new(1));
        let snapshot = {
            let mut state = executor.preservation.lock().unwrap();
            let snapshot = state.guard.take().unwrap().snapshot;
            state.published_restore = Some(snapshot.clone());
            state.publication = Some(super::stable_offer::Publication::failed(backend_failure(
                "ordered publication connection closed",
                true,
            )));
            snapshot
        };

        assert!(executor.ensure_mutation_allowed().is_err());
        let limitation = executor.limitation().unwrap();
        assert!(limitation.contains("exact restoration"));
        assert!(!limitation.contains("not retained evidence"));

        let state = executor.preservation.lock().unwrap();
        assert!(state.failure.is_some());
        assert!(state.published_restore.is_none());
        assert_eq!(
            state.fallback_restore.as_ref().unwrap().sources,
            snapshot.sources
        );
    }

    #[test]
    fn cleanup_promotes_background_failure_before_retrying_the_private_restore() {
        let executor = guarded_executor(ConnectionId::new(1), DrivingSessionId::new(1));
        let expected = {
            let mut state = executor.preservation.lock().unwrap();
            let snapshot = state.guard.take().unwrap().snapshot;
            state.published_restore = Some(snapshot.clone());
            state.publication = Some(super::stable_offer::Publication::failed(backend_failure(
                "ordered publication connection closed",
                true,
            )));
            snapshot.sources
        };

        executor
            .cleanup_with(|snapshot| {
                assert_eq!(snapshot.sources, expected);
                Ok(None)
            })
            .unwrap();

        let state = executor.preservation.lock().unwrap();
        assert!(state.failure.is_none());
        assert!(state.fallback_restore.is_none());
        assert!(state.published_restore.is_none());
    }

    #[test]
    fn a_live_guard_refuses_a_second_guard_before_wayland_access() {
        let executor = guarded_executor(ConnectionId::new(1), DrivingSessionId::new(1));
        let mut controller = Controller::new(Duration::from_secs(30));
        let owner = controller.connect();
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();

        let result = executor.execute(&Action::ClipboardPreserve, authority.cancellation());

        assert!(matches!(
            result,
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::InvalidRequest
        ));
    }

    #[test]
    fn restoration_is_scoped_to_the_guard_owner_and_driving_session() {
        let executor = guarded_executor(ConnectionId::new(9), DrivingSessionId::new(9));
        let mut controller = Controller::new(Duration::from_secs(30));
        let owner = controller.connect();
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();

        let result = executor.execute(
            &Action::ClipboardRestore {
                preservation_id: ClipboardPreservationId::new(1),
            },
            authority.cancellation(),
        );

        assert!(matches!(
            result,
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::NotLeaseOwner
        ));
    }

    #[test]
    fn keeping_is_scoped_to_the_guard_owner_and_driving_session() {
        let executor = guarded_executor(ConnectionId::new(9), DrivingSessionId::new(9));
        let mut controller = Controller::new(Duration::from_secs(30));
        let owner = controller.connect();
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();

        let result = executor.execute(
            &Action::ClipboardKeep {
                preservation_id: ClipboardPreservationId::new(1),
            },
            authority.cancellation(),
        );

        assert!(matches!(
            result,
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::NotLeaseOwner
        ));
        assert!(executor.preservation.lock().unwrap().guard.is_some());
    }

    #[test]
    fn keep_consumes_the_guard_without_restoring_and_consumed_actions_fail_closed() {
        let executor = guarded_executor(ConnectionId::new(1), DrivingSessionId::new(1));
        let mut controller = Controller::new(Duration::from_secs(30));
        let owner = controller.connect();
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();
        let keep = Action::ClipboardKeep {
            preservation_id: ClipboardPreservationId::new(1),
        };

        executor.execute(&keep, authority.cancellation()).unwrap();
        executor
            .cleanup_with(|_| panic!("a kept clipboard must not be restored at cleanup"))
            .unwrap();

        assert!(matches!(
            executor.execute(&keep, authority.cancellation()),
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::InvalidRequest
        ));
        assert!(matches!(
            executor.restore_guard_with(
                ClipboardPreservationId::new(1),
                authority.cancellation(),
                |_| panic!("a consumed guard must fail before restoration"),
            ),
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::InvalidRequest
        ));
    }

    #[test]
    fn restore_consumes_the_guard_and_consumed_actions_fail_closed() {
        let executor = guarded_executor(ConnectionId::new(1), DrivingSessionId::new(1));
        let mut controller = Controller::new(Duration::from_secs(30));
        let owner = controller.connect();
        let authority = controller.begin_execution(owner, Duration::ZERO).unwrap();

        executor
            .restore_guard_with(
                ClipboardPreservationId::new(1),
                authority.cancellation(),
                |_| Ok(None),
            )
            .unwrap();

        assert!(matches!(
            executor.execute(
                &Action::ClipboardKeep {
                    preservation_id: ClipboardPreservationId::new(1),
                },
                authority.cancellation(),
            ),
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::InvalidRequest
        ));
        assert!(matches!(
            executor.restore_guard_with(
                ClipboardPreservationId::new(1),
                authority.cancellation(),
                |_| panic!("a consumed guard must fail before restoration"),
            ),
            Err(ActionExecutionFailure::NotExecuted { error })
                if error.code == ErrorCode::InvalidRequest
        ));
    }

    fn representation(mime_type: &str, data: &[u8]) -> ClipboardRepresentation {
        ClipboardRepresentation {
            mime_type: mime_type.into(),
            data_base64: BASE64.encode(data),
        }
    }

    fn guarded_executor(owner: ConnectionId, session: DrivingSessionId) -> ClipboardExecutor {
        ClipboardExecutor {
            seat: "seat0".into(),
            preservation: Mutex::new(PreservationState {
                next_id: 1,
                guard: Some(PreservationGuard {
                    id: ClipboardPreservationId::new(1),
                    owner,
                    session,
                    snapshot: ClipboardSnapshot {
                        seat: "seat0".into(),
                        sources: vec![("text/plain".into(), b"not retained evidence".to_vec())],
                    },
                }),
                fallback_restore: None,
                published_restore: None,
                publication: None,
                failure: None,
            }),
        }
    }
}
