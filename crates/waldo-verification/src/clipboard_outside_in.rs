use std::{
    fs,
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use waldo_clipboard::{ClipboardExecutor, ClipboardSnapshot};
use waldo_verification::{PrivacyScan, scan_private_root};

use crate::outside_in::{
    McpClient, Options, RuntimeHarness, accessibility_action, accessibility_handle_with_role,
    focused_output, observe_accessibility, observe_accessibility_transient,
    observe_without_accessibility, sha256_bytes, write_json,
};

const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct ClipboardSummary {
    atomic_offer: AtomicOfferEvidence,
    direct_values: DirectValueEvidence,
    preservation: PreservationEvidence,
    unpreservable_prior_offer: UnpreservableOfferEvidence,
    text_routes: Vec<TextRouteEvidence>,
    cancellation: CancellationEvidence,
    cleanup: CleanupEvidence,
    keep: KeepEvidence,
    controller_socket_removed: bool,
    privacy_scan: PrivacyScan,
    generated_payload_leaks: usize,
    sensitive_payloads_retained: bool,
}

#[derive(Debug, Serialize)]
struct AtomicOfferEvidence {
    advertised_mime_order: Vec<String>,
    requested_order_preserved: bool,
    representations: Vec<RepresentationEvidence>,
    invalid_mime_refused_without_mutation: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct RepresentationEvidence {
    mime_type: String,
    bytes: usize,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct DirectValueEvidence {
    utf8_exact: bool,
    nul_binary_exact: bool,
    empty_exact: bool,
    maximum_exact: bool,
    over_maximum_refused_without_mutation: bool,
}

#[derive(Debug, Serialize)]
struct PreservationEvidence {
    before: Vec<RepresentationEvidence>,
    after_explicit_restore: Vec<RepresentationEvidence>,
    external_source_copy_consumed: bool,
    masked_destination_digest_matched: bool,
    submission_accepted: bool,
    paste_application_outcome: String,
    submit_application_outcome: String,
    exact_after_explicit_restore: bool,
}

#[derive(Debug, Serialize)]
struct TextRouteEvidence {
    scenario: String,
    receipt_status: String,
    route: Option<String>,
    application_outcome: String,
    destination_digest_matched: bool,
    key_chords: u64,
    insert_attempts: u64,
    successful_application_outcome: bool,
}

#[derive(Debug, Serialize)]
struct CancellationEvidence {
    receipt_status: String,
    application_outcome: String,
    stop_released_session: bool,
    destination_digest_matched: bool,
    exact_after_cleanup: bool,
}

#[derive(Debug, Serialize)]
struct UnpreservableOfferEvidence {
    receipt_status: String,
    key_chords_unchanged: bool,
    clipboard_unchanged: bool,
}

#[derive(Debug, Serialize)]
struct CleanupEvidence {
    exact_after_success: bool,
    exact_after_failure: bool,
    exact_after_disconnect: bool,
}

#[derive(Debug, Serialize)]
struct KeepEvidence {
    exact_before_cleanup: bool,
    exact_after_cleanup: bool,
    repeated_keep_failed_without_mutation: bool,
    restore_after_keep_failed_without_mutation: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct FixtureState {
    source_title: String,
    destination_title: String,
    source_digest: String,
    scenario: String,
    control_sequence: u64,
    copy_count: u64,
    key_chords: u64,
    insert_attempts: u64,
    destination_mask: String,
    destination_digest: String,
    digest_matches: bool,
    submission_count: u64,
    accepted: bool,
}

struct FixtureProcess(Child);

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct TransientDirectory(PathBuf);

impl TransientDirectory {
    fn create(seed: u64) -> Result<Self, Box<dyn std::error::Error>> {
        Self::create_with_shared_memory(seed, Path::new("/dev/shm"))
    }

    fn create_with_shared_memory(
        seed: u64,
        shared_memory: &Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let base = if shared_memory.is_dir() {
            shared_memory.to_path_buf()
        } else {
            std::env::temp_dir()
        };
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = base.join(format!(
            "waldo-clipboard-{}-{seed:x}-{nonce}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    fn remove(&mut self) -> Result<(), std::io::Error> {
        if self.0.exists() {
            fs::remove_dir_all(&self.0)?;
        }
        Ok(())
    }
}

impl Drop for TransientDirectory {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

struct ClipboardRecovery {
    executor: ClipboardExecutor,
    snapshot: Option<ClipboardSnapshot>,
}

impl ClipboardRecovery {
    fn capture() -> Result<Self, Box<dyn std::error::Error>> {
        let executor = ClipboardExecutor::initialise()
            .map_err(|error| format!("initialise clipboard recovery: {}", error.message))?;
        let snapshot = executor
            .snapshot()
            .map_err(|error| format!("capture clipboard recovery: {}", error.message))?;
        Ok(Self {
            executor,
            snapshot: Some(snapshot),
        })
    }

    fn restore(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.executor
            .restore_temporary(
                self.snapshot
                    .as_ref()
                    .ok_or("clipboard recovery was already discharged")?
                    .clone(),
            )
            .map_err(|error| format!("restore clipboard recovery: {}", error.message).into())
    }

    fn discharge(&mut self) {
        self.snapshot = None;
    }
}

impl Drop for ClipboardRecovery {
    /// The last resort when neither the run nor its error path restored
    /// the clipboard; a failure here can only be printed.
    fn drop(&mut self) {
        if let Some(snapshot) = self.snapshot.take()
            && let Err(error) = self.executor.restore_temporary(snapshot)
        {
            eprintln!(
                "waldo-verification: the operator's clipboard could not be restored: {}",
                error.message
            );
        }
    }
}

pub fn run(options: Options) -> Result<ClipboardSummary, Box<dyn std::error::Error>> {
    let mut recovery = ClipboardRecovery::capture()?;
    match run_recoverable(options, &mut recovery) {
        Ok(summary) => Ok(summary),
        // The operator's clipboard is restored on every failure, and a
        // failed restoration is reported beside the failure that caused it.
        Err(error) => match recovery.restore() {
            Ok(()) => {
                recovery.discharge();
                Err(error)
            }
            Err(restoration) => {
                recovery.discharge();
                Err(format!(
                    "{error}; restoring the operator's clipboard also failed: {restoration}"
                )
                .into())
            }
        },
    }
}

fn run_recoverable(
    options: Options,
    recovery: &mut ClipboardRecovery,
) -> Result<ClipboardSummary, Box<dyn std::error::Error>> {
    let mut harness = RuntimeHarness::start(&options)?;
    let mut transient = TransientDirectory::create(options.seed)?;
    let state_path = transient.0.join("fixture-state.json");
    let control_path = transient.0.join("fixture-control.json");
    let secret_path = transient.0.join("source-secret");
    write_json(
        control_path.clone(),
        &json!({"sequence": 0, "scenario": "accept", "expected_digest": "fixture-owned"}),
    )?;
    let mut fixture = FixtureProcess(
        Command::new(&options.python)
            .arg(&options.fixture)
            .args(["--state", path_text(&state_path)?])
            .args(["--control", path_text(&control_path)?])
            .args(["--secret-handoff", path_text(&secret_path)?])
            .args(["--seed", &options.seed.to_string()])
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let initial_fixture = wait_for_fixture(&state_path, |_| true)?;
    let source_secret = read_and_remove_secret(&secret_path)?;
    if sha256_bytes(&source_secret)? != initial_fixture.source_digest {
        return Err("fixture secret handoff did not match its published digest".into());
    }
    let output_id = focused_output()?;
    let seat = waldo_clipboard::regular_clipboard_seat()
        .map_err(|error| format!("discover regular clipboard seat: {}", error.message))?;
    let before = clipboard_inventory(&seat)?;
    let initial_observation = observe_without_accessibility(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.destination_title,
        &options.evidence_directory,
        "clipboard-initial",
    )?;
    let mut clipboard_observation_id = initial_observation.id;
    let preserved = act_chained(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        vec![json!({"type": "clipboard_preserve"})],
        Vec::new(),
    )?;
    require_receipt(&preserved, "executed")?;
    write_stage(&options.evidence_directory, "clipboard_preserved")?;
    let preservation_id = first_output(&preserved)["preservation_id"]
        .as_u64()
        .ok_or("ClipboardPreserve returned no preservation id")?;

    let html = format!("<b>Waldo {:x}</b>", options.seed);
    let plain = format!("Waldo plain {:x}", options.seed);
    let mut generated_payloads = vec![
        source_secret,
        html.as_bytes().to_vec(),
        plain.as_bytes().to_vec(),
    ];
    let atomic_action = clipboard_offer(&[
        ("text/html", html.as_bytes()),
        ("text/plain", plain.as_bytes()),
    ]);
    let atomic = act_chained_retry_focus_once(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        vec![atomic_action.clone()],
        Vec::new(),
        &output_id,
        &initial_fixture.destination_title,
        &options.evidence_directory,
    )?;
    require_receipt(&atomic, "executed")?;
    let atomic_inventory = clipboard_inventory(&seat)?;
    let advertised_atomic_order = atomic_inventory
        .iter()
        .map(|representation| representation.mime_type.clone())
        .collect::<Vec<_>>();
    let requested_atomic_order = ["text/html", "text/plain"];
    let requested_order_preserved = advertised_atomic_order
        .iter()
        .map(String::as_str)
        .eq(requested_atomic_order);
    let atomic_round_trip = read_representations(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        &["text/html", "text/plain"],
    )?;
    let expected_atomic = evidence_for(&[
        ("text/html", html.as_bytes()),
        ("text/plain", plain.as_bytes()),
    ])?;
    if atomic_round_trip != expected_atomic {
        return Err("atomic MIME representations did not round-trip exactly".into());
    }
    write_stage(&options.evidence_directory, "atomic_offer_verified")?;

    let invalid = act_chained(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        vec![clipboard_offer(&[("", b"invalid")])],
        Vec::new(),
    )?;
    let invalid_refused =
        first_status(&invalid) == "failed" && clipboard_inventory(&seat)? == atomic_inventory;
    write_stage(&options.evidence_directory, "invalid_offer_refused")?;

    let utf8 = format!("Grüße £ {:x}", options.seed);
    let binary = b"A\0B\0C".to_vec();
    let empty = Vec::new();
    generated_payloads.extend([utf8.as_bytes().to_vec(), binary.clone(), empty.clone()]);
    let utf8_exact = write_and_read(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        "text/plain;charset=utf-8",
        utf8.as_bytes(),
    )?;
    let nul_binary_exact = write_and_read(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        "application/octet-stream",
        &binary,
    )?;
    let empty_exact = write_and_read(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        "application/x-waldo-empty",
        &empty,
    )?;
    write_stage(&options.evidence_directory, "direct_values_verified")?;

    let maximum = vec![b'Z'; MAX_CLIPBOARD_BYTES];
    let maximum_digest = sha256_bytes(&maximum)?;
    let maximum_exact = write_and_read_digest(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        "application/x-waldo-boundary",
        &maximum,
    )? == maximum_digest;
    let over_maximum = vec![b'Y'; MAX_CLIPBOARD_BYTES + 1];
    let over = act_chained(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        vec![clipboard_write(
            "application/x-waldo-boundary",
            &over_maximum,
        )],
        Vec::new(),
    )?;
    let over_refused = first_status(&over) == "failed"
        && read_digest(
            &mut harness.mcp,
            &mut clipboard_observation_id,
            "application/x-waldo-boundary",
        )? == maximum_digest;
    write_stage(&options.evidence_directory, "size_boundary_verified")?;

    let atomic_again = act_chained(
        &mut harness.mcp,
        &mut clipboard_observation_id,
        vec![atomic_action],
        Vec::new(),
    )?;
    require_receipt(&atomic_again, "executed")?;
    write_stage(&options.evidence_directory, "external_copy_ready")?;

    let source_observation = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.source_title,
        &options.evidence_directory,
        "clipboard-source",
    )?;
    let (copy_handle, copy_action) =
        accessibility_action(&source_observation, "Copy generated value")?;
    let copied = act(
        &mut harness.mcp,
        source_observation.id,
        vec![json!({
            "type": "accessibility",
            "element": copy_handle,
            "operation": {"type": "invoke", "action": copy_action}
        })],
        Vec::new(),
    )?;
    require_receipt(&copied, "executed")?;
    let copied_state = wait_for_fixture(&state_path, |state| state.copy_count >= 1)?;

    let destination_observation = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.destination_title,
        &options.evidence_directory,
        "clipboard-destination",
    )?;
    let digest_handle = accessibility_handle_with_role(
        &destination_observation,
        "Destination digest matched",
        "checkbox",
    )?;
    let pasted = act(
        &mut harness.mcp,
        destination_observation.id,
        vec![json!({"type": "key_chord", "keys": ["Control", "v"]})],
        vec![json!({
            "type": "element_state",
            "element": digest_handle,
            "state": "checked",
            "present": true
        })],
    )?;
    let pasted_state = wait_for_fixture(&state_path, |state| state.digest_matches)?;
    let submit_observation = observe_accessibility_transient(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.destination_title,
        &transient.0,
    )?;
    let (submit_handle, submit_action) =
        accessibility_action(&submit_observation, "Submit masked destination")?;
    let accepted_handle =
        accessibility_handle_with_role(&submit_observation, "Submission accepted", "checkbox")?;
    let submitted = act(
        &mut harness.mcp,
        submit_observation.id,
        vec![json!({
            "type": "accessibility",
            "element": submit_handle,
            "operation": {"type": "invoke", "action": submit_action}
        })],
        vec![json!({
            "type": "element_state",
            "element": accepted_handle,
            "state": "checked",
            "present": true
        })],
    )?;
    let submitted_state = wait_for_fixture(&state_path, |state| state.accepted)?;

    let restore_observation = observe_without_accessibility(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.destination_title,
        &options.evidence_directory,
        "clipboard-before-restore",
    )?;
    let restored = act(
        &mut harness.mcp,
        restore_observation.id,
        vec![json!({
            "type": "clipboard_restore",
            "preservation_id": preservation_id
        })],
        Vec::new(),
    )?;
    require_receipt(&restored, "executed")?;
    let after_explicit_restore = clipboard_inventory(&seat)?;
    write_stage(&options.evidence_directory, "explicit_restore_verified")?;

    let unpreservable_mime = "application/x-waldo-unpreservable";
    let unpreservable_expected = evidence_for(&[(unpreservable_mime, &over_maximum)])?;
    let mut unpreservable_owner = FixtureProcess(
        Command::new("wl-copy")
            .args([
                "--foreground",
                "--seat",
                &seat,
                "--type",
                unpreservable_mime,
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    unpreservable_owner
        .0
        .stdin
        .take()
        .ok_or("wl-copy did not expose standard input")?
        .write_all(&over_maximum)?;
    wait_for_clipboard(&seat, &unpreservable_expected)?;
    let before_unpreservable = wait_for_fixture(&state_path, |_| true)?;
    let unpreservable_observation = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &before_unpreservable.destination_title,
        &options.evidence_directory,
        "clipboard-unpreservable",
    )?;
    let refused_payload = format!("unpreservable-{:x}", options.seed);
    generated_payloads.push(refused_payload.as_bytes().to_vec());
    let unpreservable = act(
        &mut harness.mcp,
        unpreservable_observation.id,
        vec![json!({
            "type": "text",
            "text": refused_payload,
            "route": "clipboard_paste"
        })],
        Vec::new(),
    )?;
    thread::sleep(Duration::from_millis(100));
    let after_unpreservable = wait_for_fixture(&state_path, |_| true)?;
    let unpreservable_inventory = clipboard_inventory(&seat)?;
    unpreservable_owner.0.kill()?;
    unpreservable_owner.0.wait()?;
    recovery.restore()?;
    wait_for_clipboard(&seat, &before)?;
    write_stage(
        &options.evidence_directory,
        "unpreservable_offer_refused_before_mutation",
    )?;

    let mut text_routes = Vec::new();
    for (index, scenario) in ["accept", "rejected", "unchanged", "blocked"]
        .into_iter()
        .enumerate()
    {
        let payload = format!("route-{scenario}-{:x}-{}", options.seed, index);
        generated_payloads.push(payload.as_bytes().to_vec());
        let expected_digest = sha256_bytes(payload.as_bytes())?;
        set_control(&control_path, index as u64 + 1, scenario, &expected_digest)?;
        let configured = wait_for_fixture(&state_path, |state| {
            state.control_sequence == index as u64 + 1 && state.scenario == scenario
        })?;
        let observed = observe_accessibility(
            &mut harness.mcp,
            &output_id,
            &configured.destination_title,
            &options.evidence_directory,
            &format!("clipboard-route-{scenario}"),
        )?;
        let status_handle =
            accessibility_handle_with_role(&observed, "Destination digest matched", "checkbox")?;
        let response = act(
            &mut harness.mcp,
            observed.id,
            vec![json!({
                "type": "text",
                "text": payload,
                "route": "clipboard_paste"
            })],
            vec![json!({
                "type": "element_state",
                "element": status_handle,
                "state": "checked",
                "present": true
            })],
        )?;
        let final_state = wait_for_fixture(&state_path, |state| {
            state.control_sequence == index as u64 + 1 && state.key_chords > configured.key_chords
        })?;
        let application_outcome = application_outcome(&response);
        text_routes.push(TextRouteEvidence {
            scenario: scenario.into(),
            receipt_status: first_status(&response).into(),
            route: first_route(&response),
            successful_application_outcome: application_outcome == "completed",
            application_outcome,
            destination_digest_matched: final_state.digest_matches,
            key_chords: final_state.key_chords - configured.key_chords,
            insert_attempts: final_state.insert_attempts - configured.insert_attempts,
        });
    }
    for evidence in text_routes.iter().filter(|case| case.scenario != "accept") {
        if let Some(contradiction) = negative_case_contradiction(
            &evidence.receipt_status,
            &evidence.application_outcome,
            evidence.destination_digest_matched,
        ) {
            return Err(format!("{} destination: {contradiction}", evidence.scenario).into());
        }
    }
    write_stage(&options.evidence_directory, "text_routes_verified")?;
    let accept_route_passed = text_routes.iter().any(|case| {
        case.scenario == "accept"
            && case.receipt_status == "executed"
            && case.route.as_deref() == Some("clipboard_paste")
            && accepted_application_outcome(&case.application_outcome)
            && case.destination_digest_matched
    });

    let cancellation_before = clipboard_inventory(&seat)?;
    let cancellation_observation = observe_without_accessibility(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.destination_title,
        &options.evidence_directory,
        "clipboard-cancellation",
    )?;
    let cancellation_guard = act(
        &mut harness.mcp,
        cancellation_observation.id,
        vec![json!({"type": "clipboard_preserve"})],
        Vec::new(),
    )?;
    require_receipt(&cancellation_guard, "executed")?;
    let cancelled_payload = format!("cancelled-{:x}", options.seed);
    generated_payloads.push(cancelled_payload.as_bytes().to_vec());
    let cancelled_digest = sha256_bytes(cancelled_payload.as_bytes())?;
    set_control(&control_path, 10, "blocked", &cancelled_digest)?;
    let configured = wait_for_fixture(&state_path, |state| state.control_sequence == 10)?;
    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &configured.destination_title,
        &options.evidence_directory,
        "clipboard-cancellation-ready",
    )?;
    let status_handle =
        accessibility_handle_with_role(&observed, "Destination digest matched", "checkbox")?;
    let act_id = harness.mcp.send(
        "tools/call",
        json!({
            "name": "act",
            "arguments": {
                "observation_id": observed.id,
                "actions": [{
                    "type": "text",
                    "text": cancelled_payload,
                    "route": "clipboard_paste"
                }],
                "verification": [{
                    "type": "element_state",
                    "element": status_handle,
                    "state": "checked",
                    "present": true
                }]
            }
        }),
    )?;
    let _ = wait_for_fixture(&state_path, |state| {
        state.control_sequence == 10 && state.key_chords > configured.key_chords
    })?;
    let stop_id = harness
        .mcp
        .send("tools/call", json!({"name": "stop", "arguments": {}}))?;
    let stop_response = harness
        .mcp
        .receive(stop_id, Duration::from_secs(10))?
        .ok_or("cancel Stop exceeded verifier deadline")?;
    let cancelled = harness
        .mcp
        .receive(act_id, Duration::from_secs(10))?
        .ok_or("cancelled Act exceeded verifier deadline")?;
    let cancellation_state = wait_for_fixture(&state_path, |state| state.control_sequence == 10)?;
    let cancellation_after = clipboard_inventory(&seat)?;
    let cancellation_outcome = application_outcome(&cancelled);
    if let Some(contradiction) = negative_case_contradiction(
        first_status(&cancelled),
        &cancellation_outcome,
        cancellation_state.digest_matches,
    ) {
        return Err(format!("cancelled destination: {contradiction}").into());
    }
    write_stage(&options.evidence_directory, "cancellation_cleanup_verified")?;

    recovery.restore()?;
    wait_for_clipboard(&seat, &before)?;
    let success_payload = format!("cleanup-success-{:x}", options.seed).into_bytes();
    let failure_payload = format!("cleanup-failure-{:x}", options.seed).into_bytes();
    let disconnect_payload = format!("cleanup-disconnect-{:x}", options.seed).into_bytes();
    generated_payloads.extend([
        success_payload.clone(),
        failure_payload.clone(),
        disconnect_payload.clone(),
    ]);
    let cleanup = CleanupContext {
        options: &options,
        output_id: &output_id,
        fixture_title: &initial_fixture.destination_title,
        seat: &seat,
        baseline: &before,
    };
    let exact_after_success = cleanup.exercise(
        &mut harness,
        "success",
        &success_payload,
        CleanupExit::StopAfterSuccess,
    )?;
    let exact_after_failure = cleanup.exercise(
        &mut harness,
        "failure",
        &failure_payload,
        CleanupExit::StopAfterFailure,
    )?;
    let exact_after_disconnect = cleanup.exercise(
        &mut harness,
        "disconnect",
        &disconnect_payload,
        CleanupExit::Disconnect,
    )?;

    let keep_html = format!("<i>kept-{:x}</i>", options.seed);
    let keep_plain = format!("kept-{:x}", options.seed);
    generated_payloads.extend([
        keep_html.as_bytes().to_vec(),
        keep_plain.as_bytes().to_vec(),
    ]);
    let keep_expected = evidence_for(&[
        ("text/html", keep_html.as_bytes()),
        ("text/plain", keep_plain.as_bytes()),
    ])?;
    let keep_observation = observe_without_accessibility(
        &mut harness.mcp,
        &output_id,
        &initial_fixture.destination_title,
        &options.evidence_directory,
        "clipboard-keep",
    )?;
    let mut keep_observation_id = keep_observation.id;
    let keep_guard = act_chained(
        &mut harness.mcp,
        &mut keep_observation_id,
        vec![json!({"type": "clipboard_preserve"})],
        Vec::new(),
    )?;
    require_receipt(&keep_guard, "executed")?;
    let keep_id = first_output(&keep_guard)["preservation_id"]
        .as_u64()
        .ok_or("ClipboardPreserve returned no preservation id for Keep")?;
    let keep_write = act_chained(
        &mut harness.mcp,
        &mut keep_observation_id,
        vec![clipboard_offer(&[
            ("text/html", keep_html.as_bytes()),
            ("text/plain", keep_plain.as_bytes()),
        ])],
        Vec::new(),
    )?;
    require_receipt(&keep_write, "executed")?;
    let kept = act_chained(
        &mut harness.mcp,
        &mut keep_observation_id,
        vec![json!({
            "type": "clipboard_keep",
            "preservation_id": keep_id
        })],
        Vec::new(),
    )?;
    require_receipt(&kept, "executed")?;
    let exact_before_keep_cleanup = clipboard_inventory(&seat)? == keep_expected;
    let repeated_keep = act_chained(
        &mut harness.mcp,
        &mut keep_observation_id,
        vec![json!({
            "type": "clipboard_keep",
            "preservation_id": keep_id
        })],
        Vec::new(),
    )?;
    let repeated_keep_failed_without_mutation =
        first_status(&repeated_keep) == "failed" && clipboard_inventory(&seat)? == keep_expected;
    let restore_after_keep = act_chained(
        &mut harness.mcp,
        &mut keep_observation_id,
        vec![json!({
            "type": "clipboard_restore",
            "preservation_id": keep_id
        })],
        Vec::new(),
    )?;
    let restore_after_keep_failed_without_mutation = first_status(&restore_after_keep) == "failed"
        && clipboard_inventory(&seat)? == keep_expected;
    harness
        .mcp
        .call_with_timeout(
            "tools/call",
            json!({"name": "stop", "arguments": {}}),
            Duration::from_secs(10),
        )?
        .ok_or("Keep cleanup Stop exceeded verifier deadline")?;
    let exact_after_keep_cleanup = wait_for_clipboard(&seat, &keep_expected).is_ok();
    write_stage(&options.evidence_directory, "keep_cleanup_verified")?;

    recovery.restore()?;
    wait_for_clipboard(&seat, &before)?;
    recovery.discharge();

    fixture.0.kill()?;
    fixture.0.wait()?;
    transient.remove()?;
    let (_, controller_socket_removed) = harness.shutdown()?;
    let payloads: Vec<_> = generated_payloads
        .iter()
        .map(|payload| ("generated-clipboard-payload", payload.as_slice()))
        .collect();
    let privacy_scan = scan_private_root(&options.evidence_directory, &payloads)?;
    let leaks = privacy_scan.leaks.len();
    let summary = ClipboardSummary {
        atomic_offer: AtomicOfferEvidence {
            advertised_mime_order: advertised_atomic_order,
            requested_order_preserved,
            representations: atomic_round_trip,
            invalid_mime_refused_without_mutation: invalid_refused,
        },
        direct_values: DirectValueEvidence {
            utf8_exact,
            nul_binary_exact,
            empty_exact,
            maximum_exact,
            over_maximum_refused_without_mutation: over_refused,
        },
        preservation: PreservationEvidence {
            before: before.clone(),
            after_explicit_restore: after_explicit_restore.clone(),
            external_source_copy_consumed: copied_state.copy_count >= 1,
            masked_destination_digest_matched: pasted_state.digest_matches
                && !pasted_state.destination_mask.is_empty()
                && pasted_state.destination_digest == initial_fixture.source_digest,
            submission_accepted: submitted_state.accepted && submitted_state.submission_count >= 1,
            paste_application_outcome: application_outcome(&pasted),
            submit_application_outcome: application_outcome(&submitted),
            exact_after_explicit_restore: before == after_explicit_restore,
        },
        unpreservable_prior_offer: UnpreservableOfferEvidence {
            receipt_status: first_status(&unpreservable).into(),
            key_chords_unchanged: before_unpreservable.key_chords == after_unpreservable.key_chords,
            clipboard_unchanged: unpreservable_inventory == unpreservable_expected,
        },
        text_routes,
        cancellation: CancellationEvidence {
            receipt_status: first_status(&cancelled).into(),
            application_outcome: cancellation_outcome,
            stop_released_session:
                stop_response["result"]["structuredContent"]["result"]["released_session"]
                    .is_number(),
            destination_digest_matched: cancellation_state.digest_matches,
            exact_after_cleanup: cancellation_before == cancellation_after,
        },
        cleanup: CleanupEvidence {
            exact_after_success,
            exact_after_failure,
            exact_after_disconnect,
        },
        keep: KeepEvidence {
            exact_before_cleanup: exact_before_keep_cleanup,
            exact_after_cleanup: exact_after_keep_cleanup,
            repeated_keep_failed_without_mutation,
            restore_after_keep_failed_without_mutation,
        },
        controller_socket_removed,
        privacy_scan: privacy_scan.clone(),
        generated_payload_leaks: leaks,
        sensitive_payloads_retained: leaks != 0,
    };
    write_json(options.evidence_directory.join("summary.json"), &summary)?;
    write_json(
        options.evidence_directory.join("privacy-scan.json"),
        &privacy_scan,
    )?;
    if !summary.atomic_offer.requested_order_preserved
        || !summary.atomic_offer.invalid_mime_refused_without_mutation
        || !summary.direct_values.utf8_exact
        || !summary.direct_values.nul_binary_exact
        || !summary.direct_values.empty_exact
        || !summary.direct_values.maximum_exact
        || !summary.direct_values.over_maximum_refused_without_mutation
        || !summary.preservation.exact_after_explicit_restore
        || !summary.preservation.external_source_copy_consumed
        || !summary.preservation.masked_destination_digest_matched
        || !summary.preservation.submission_accepted
        || !accepted_application_outcome(&summary.preservation.paste_application_outcome)
        || !accepted_application_outcome(&summary.preservation.submit_application_outcome)
        || summary.unpreservable_prior_offer.receipt_status != "failed"
        || !summary.unpreservable_prior_offer.key_chords_unchanged
        || !summary.unpreservable_prior_offer.clipboard_unchanged
        || !accept_route_passed
        || !summary.cancellation.stop_released_session
        || !summary.cancellation.exact_after_cleanup
        || !summary.cleanup.exact_after_success
        || !summary.cleanup.exact_after_failure
        || !summary.cleanup.exact_after_disconnect
        || !summary.keep.exact_before_cleanup
        || !summary.keep.exact_after_cleanup
        || !summary.keep.repeated_keep_failed_without_mutation
        || !summary.keep.restore_after_keep_failed_without_mutation
        || !summary.controller_socket_removed
        || summary.generated_payload_leaks != 0
    {
        return Err("clipboard outside-in acceptance invariant failed".into());
    }
    Ok(summary)
}

enum CleanupExit {
    StopAfterSuccess,
    StopAfterFailure,
    Disconnect,
}

struct CleanupContext<'a> {
    options: &'a Options,
    output_id: &'a str,
    fixture_title: &'a str,
    seat: &'a str,
    baseline: &'a [RepresentationEvidence],
}

impl CleanupContext<'_> {
    fn exercise(
        &self,
        harness: &mut RuntimeHarness,
        label: &str,
        payload: &[u8],
        exit: CleanupExit,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let observation = observe_without_accessibility(
            &mut harness.mcp,
            self.output_id,
            self.fixture_title,
            &self.options.evidence_directory,
            &format!("clipboard-cleanup-{label}"),
        )?;
        let mut observation_id = observation.id;
        let preserve = act_chained(
            &mut harness.mcp,
            &mut observation_id,
            vec![json!({"type": "clipboard_preserve"})],
            Vec::new(),
        )?;
        require_receipt(&preserve, "executed")?;
        let mime_type = format!("application/x-waldo-cleanup-{label}");
        let write = act_chained(
            &mut harness.mcp,
            &mut observation_id,
            vec![clipboard_write(&mime_type, payload)],
            Vec::new(),
        )?;
        require_receipt(&write, "executed")?;
        let provisional = evidence_for(&[(mime_type.as_str(), payload)])?;
        wait_for_clipboard(self.seat, &provisional)?;

        match exit {
            CleanupExit::StopAfterSuccess => {
                let read = act_chained(
                    &mut harness.mcp,
                    &mut observation_id,
                    vec![json!({"type": "clipboard_read", "mime_type": mime_type})],
                    Vec::new(),
                )?;
                require_receipt(&read, "executed")?;
                harness
                    .mcp
                    .call_with_timeout(
                        "tools/call",
                        json!({"name": "stop", "arguments": {}}),
                        Duration::from_secs(10),
                    )?
                    .ok_or("successful cleanup Stop exceeded verifier deadline")?;
            }
            CleanupExit::StopAfterFailure => {
                let failed = act_chained(
                    &mut harness.mcp,
                    &mut observation_id,
                    vec![clipboard_write("", b"invalid")],
                    Vec::new(),
                )?;
                if first_status(&failed) != "failed"
                    || clipboard_inventory(self.seat)? != provisional
                {
                    return Err("failed guarded action changed the provisional clipboard".into());
                }
                harness
                    .mcp
                    .call_with_timeout(
                        "tools/call",
                        json!({"name": "stop", "arguments": {}}),
                        Duration::from_secs(10),
                    )?
                    .ok_or("failed-action cleanup Stop exceeded verifier deadline")?;
            }
            CleanupExit::Disconnect => harness.restart_mcp(&self.options.mcp)?,
        }

        wait_for_clipboard(self.seat, self.baseline)?;
        write_stage(
            &self.options.evidence_directory,
            &format!("{label}_cleanup_verified"),
        )?;
        Ok(clipboard_inventory(self.seat)? == self.baseline)
    }
}

fn act(
    mcp: &mut McpClient,
    observation_id: u64,
    actions: Vec<Value>,
    verification: Vec<Value>,
) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(mcp
        .call_with_timeout(
            "tools/call",
            json!({
                "name": "act",
                "arguments": {
                    "observation_id": observation_id,
                    "actions": actions,
                    "verification": verification
                }
            }),
            Duration::from_secs(30),
        )?
        .ok_or("clipboard Act exceeded verifier deadline")?)
}

fn act_chained(
    mcp: &mut McpClient,
    observation_id: &mut u64,
    actions: Vec<Value>,
    verification: Vec<Value>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = act(mcp, *observation_id, actions, verification)?;
    if let Some(next) = result(&response)["observation"]["id"].as_u64() {
        *observation_id = next;
    }
    Ok(response)
}

fn act_chained_retry_focus_once(
    mcp: &mut McpClient,
    observation_id: &mut u64,
    actions: Vec<Value>,
    verification: Vec<Value>,
    output_id: &str,
    fixture_title: &str,
    evidence_directory: &Path,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = act_chained(mcp, observation_id, actions.clone(), verification.clone())?;
    if !is_pre_dispatch_focus_changed(&response) {
        return Ok(response);
    }

    let observation = observe_without_accessibility(
        mcp,
        output_id,
        fixture_title,
        evidence_directory,
        "clipboard-focus-reground",
    )?;
    *observation_id = observation.id;
    act_chained(mcp, observation_id, actions, verification)
}

fn is_pre_dispatch_focus_changed(response: &Value) -> bool {
    response["result"]["isError"].as_bool() == Some(true)
        && response["result"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["text"].as_str())
            .any(|text| text.split_once(':').map(|(code, _)| code) == Some("focus_changed"))
}

fn clipboard_offer(representations: &[(&str, &[u8])]) -> Value {
    json!({
        "type": "clipboard_write_offer",
        "representations": representations.iter().map(|(mime_type, data)| json!({
            "mime_type": mime_type,
            "data_base64": BASE64.encode(data)
        })).collect::<Vec<_>>()
    })
}

fn clipboard_write(mime_type: &str, data: &[u8]) -> Value {
    json!({
        "type": "clipboard_write",
        "mime_type": mime_type,
        "data_base64": BASE64.encode(data)
    })
}

fn write_and_read(
    mcp: &mut McpClient,
    observation_id: &mut u64,
    mime_type: &str,
    data: &[u8],
) -> Result<bool, Box<dyn std::error::Error>> {
    Ok(write_and_read_digest(mcp, observation_id, mime_type, data)? == sha256_bytes(data)?)
}

fn write_and_read_digest(
    mcp: &mut McpClient,
    observation_id: &mut u64,
    mime_type: &str,
    data: &[u8],
) -> Result<String, Box<dyn std::error::Error>> {
    let response = act_chained(
        mcp,
        observation_id,
        vec![clipboard_write(mime_type, data)],
        Vec::new(),
    )?;
    require_receipt(&response, "executed")?;
    read_digest(mcp, observation_id, mime_type)
}

fn read_digest(
    mcp: &mut McpClient,
    observation_id: &mut u64,
    mime_type: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let response = act_chained(
        mcp,
        observation_id,
        vec![json!({"type": "clipboard_read", "mime_type": mime_type})],
        Vec::new(),
    )?;
    require_receipt(&response, "executed")?;
    let data = BASE64.decode(
        first_output(&response)["data_base64"]
            .as_str()
            .ok_or("ClipboardRead returned no base64 data")?,
    )?;
    sha256_bytes(&data)
}

fn read_representations(
    mcp: &mut McpClient,
    observation_id: &mut u64,
    mime_types: &[&str],
) -> Result<Vec<RepresentationEvidence>, Box<dyn std::error::Error>> {
    let mut evidence = Vec::new();
    for mime_type in mime_types {
        let response = act_chained(
            mcp,
            observation_id,
            vec![json!({"type": "clipboard_read", "mime_type": mime_type})],
            Vec::new(),
        )?;
        require_receipt(&response, "executed")?;
        let data = BASE64.decode(
            first_output(&response)["data_base64"]
                .as_str()
                .ok_or("ClipboardRead returned no base64 data")?,
        )?;
        evidence.push(RepresentationEvidence {
            mime_type: (*mime_type).into(),
            bytes: data.len(),
            sha256: sha256_bytes(&data)?,
        });
    }
    Ok(evidence)
}

fn clipboard_inventory(
    seat: &str,
) -> Result<Vec<RepresentationEvidence>, Box<dyn std::error::Error>> {
    let listed = Command::new("wl-paste")
        .args(["--seat", seat, "--list-types"])
        .output()?;
    if !listed.status.success() {
        // wl-paste exits unsuccessfully both for an empty selection and for
        // a failure to read one; only its empty-selection message is empty.
        let stderr = String::from_utf8_lossy(&listed.stderr);
        if stderr.contains("Nothing is copied") {
            return Ok(Vec::new());
        }
        return Err(format!("wl-paste --list-types failed: {}", stderr.trim()).into());
    }
    let mut evidence = Vec::new();
    for mime_type in String::from_utf8(listed.stdout)?.lines() {
        let output = Command::new("wl-paste")
            .args(["--seat", seat, "--no-newline", "--type", mime_type])
            .output()?;
        if !output.status.success() {
            return Err(format!("wl-paste failed for advertised MIME {mime_type:?}").into());
        }
        evidence.push(RepresentationEvidence {
            mime_type: mime_type.into(),
            bytes: output.stdout.len(),
            sha256: sha256_bytes(&output.stdout)?,
        });
    }
    Ok(evidence)
}

fn wait_for_clipboard(
    seat: &str,
    expected: &[RepresentationEvidence],
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if clipboard_inventory(seat)? == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the expected clipboard inventory".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn evidence_for(
    representations: &[(&str, &[u8])],
) -> Result<Vec<RepresentationEvidence>, Box<dyn std::error::Error>> {
    representations
        .iter()
        .map(|(mime_type, data)| {
            Ok(RepresentationEvidence {
                mime_type: (*mime_type).into(),
                bytes: data.len(),
                sha256: sha256_bytes(data)?,
            })
        })
        .collect()
}

fn result(response: &Value) -> &Value {
    &response["result"]["structuredContent"]["result"]
}

fn first_status(response: &Value) -> &str {
    result(response)["receipts"][0]["status"]
        .as_str()
        .unwrap_or("missing")
}

fn first_route(response: &Value) -> Option<String> {
    result(response)["receipts"][0]["route"]
        .as_str()
        .map(str::to_owned)
}

fn first_output(response: &Value) -> &Value {
    &result(response)["receipts"][0]["output"]
}

fn application_outcome(response: &Value) -> String {
    result(response)["application_outcome"]
        .as_str()
        .unwrap_or("missing")
        .into()
}

fn accepted_application_outcome(outcome: &str) -> bool {
    matches!(outcome, "completed" | "inconclusive")
}

/// What is wrong with a case whose destination must not receive the text:
/// a malformed response, a claimed completion, or a destination that did
/// receive the expected text after all.
fn negative_case_contradiction(
    receipt_status: &str,
    application_outcome: &str,
    destination_digest_matched: bool,
) -> Option<&'static str> {
    if !matches!(
        receipt_status,
        "executed" | "failed" | "indeterminate" | "not_executed"
    ) {
        return Some("the response carried no receipt status");
    }
    if !matches!(application_outcome, "inconclusive" | "failed") {
        return Some("the application outcome was completed or missing");
    }
    if destination_digest_matched {
        return Some("the destination received the expected text");
    }
    None
}

fn require_receipt(response: &Value, expected: &str) -> Result<(), Box<dyn std::error::Error>> {
    if response["result"]["isError"].as_bool() == Some(true) {
        let message = response["result"]["content"]
            .as_array()
            .and_then(|content| content.iter().find(|item| item["type"] == "text"))
            .and_then(|item| item["text"].as_str())
            .unwrap_or("MCP returned an unstructured tool failure");
        return Err(format!("Waldo MCP Act failed: {message}").into());
    }
    let actual = first_status(response);
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "expected {expected} receipt, got {actual}: {}",
            result(response)
        )
        .into())
    }
}

fn set_control(
    path: &Path,
    sequence: u64,
    scenario: &str,
    expected_digest: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    write_json(
        path.to_owned(),
        &json!({
            "sequence": sequence,
            "scenario": scenario,
            "expected_digest": expected_digest
        }),
    )
}

fn write_stage(directory: &Path, stage: &str) -> Result<(), Box<dyn std::error::Error>> {
    write_json(
        directory.join("clipboard-stage.json"),
        &json!({"stage": stage, "payload_retained": false}),
    )
}

fn wait_for_fixture(
    path: &Path,
    predicate: impl Fn(&FixtureState) -> bool,
) -> Result<FixtureState, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(bytes) = fs::read(path)
            && let Ok(state) = serde_json::from_slice::<FixtureState>(&bytes)
            && predicate(&state)
        {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for clipboard fixture at {}",
                path.display()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn read_and_remove_secret(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let secret = loop {
        match fs::read(path) {
            Ok(secret) if !secret.is_empty() => break secret,
            Ok(_) | Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(_) => return Err("fixture secret handoff was empty".into()),
            Err(error) => return Err(format!("fixture secret handoff failed: {error}").into()),
        }
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(&vec![0; secret.len()])?;
    file.sync_all()?;
    drop(file);
    fs::remove_file(path)?;
    Ok(secret)
}

fn path_text(path: &Path) -> Result<&str, Box<dyn std::error::Error>> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path: {}", path.display()).into())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        accepted_application_outcome, is_pre_dispatch_focus_changed, negative_case_contradiction,
        require_receipt,
    };

    #[test]
    fn accepted_consumer_truth_allows_conservative_completion_but_never_failure() {
        assert!(accepted_application_outcome("completed"));
        assert!(accepted_application_outcome("inconclusive"));
        assert!(!accepted_application_outcome("failed"));
        assert!(!accepted_application_outcome("missing"));
    }

    #[test]
    fn a_negative_case_must_be_well_formed_unfinished_and_undelivered() {
        assert_eq!(
            negative_case_contradiction("executed", "inconclusive", false),
            None
        );
        assert_eq!(negative_case_contradiction("failed", "failed", false), None);
        assert!(negative_case_contradiction("missing", "inconclusive", false).is_some());
        assert!(negative_case_contradiction("executed", "missing", false).is_some());
        assert!(negative_case_contradiction("executed", "completed", false).is_some());
        assert!(negative_case_contradiction("executed", "inconclusive", true).is_some());
    }

    #[test]
    fn tool_failures_surface_the_payload_free_mcp_error_text() {
        let response = json!({
            "result": {
                "isError": true,
                "content": [{
                    "type": "text",
                    "text": "stale_observation: grounding no longer matches"
                }]
            }
        });

        let error = require_receipt(&response, "executed").unwrap_err();
        assert!(error.to_string().contains("stale_observation"));
    }

    #[test]
    fn only_the_exact_pre_dispatch_focus_code_is_retryable() {
        let focus_changed = json!({
            "result": {
                "isError": true,
                "content": [{
                    "type": "text",
                    "text": "focus_changed: foreground focus changed after the cited observation"
                }]
            }
        });
        let indeterminate = json!({
            "result": {
                "isError": false,
                "structuredContent": {"result": {"receipts": [{
                    "status": "indeterminate",
                    "error": {"code": "focus_changed"}
                }]}}
            }
        });
        let similar_code = json!({
            "result": {
                "isError": true,
                "content": [{"type": "text", "text": "focus_changed_late: uncertain"}]
            }
        });

        assert!(is_pre_dispatch_focus_changed(&focus_changed));
        assert!(!is_pre_dispatch_focus_changed(&indeterminate));
        assert!(!is_pre_dispatch_focus_changed(&similar_code));
    }
}

#[cfg(test)]
#[path = "tests/transient_defaults.rs"]
mod transient_defaults;
