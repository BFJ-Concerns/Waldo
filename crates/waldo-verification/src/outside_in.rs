use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, GenericImageView, ImageFormat};
use niri_ipc::{Action as NiriAction, Request, Response, socket::Socket};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use waldo_verification::{FixtureRect, TargetGrid, capture_niri_topology};

pub struct Options {
    pub evidence_directory: PathBuf,
    pub waldod: PathBuf,
    pub mcp: PathBuf,
    pub fixture: PathBuf,
    pub python: PathBuf,
    pub seed: u64,
}

#[derive(Debug, Serialize)]
pub struct OutsideInSummary {
    identity: IdentityEvidence,
    output_id: String,
    coordinate: CoordinateResult,
    delayed_completion: DelayedCompletionResult,
    continuous_completion: ContinuousCompletionResult,
    transparent_interception: Option<TransparentInterceptionResult>,
    contradictory_state: Option<ContradictoryStateResult>,
    controller_socket_removed: bool,
    sensitive_payloads_retained: bool,
}

#[derive(Debug, Serialize)]
pub struct AccessibilitySummary {
    identity: IdentityEvidence,
    invoke_reached_application: bool,
    set_text_reached_application: bool,
    focus_outcome: &'static str,
    focus_truthful: bool,
    set_value_identity_refresh_reached_application: bool,
    selection_reached_application: bool,
    stale_replacement_refused: bool,
    ambiguity_propagated: bool,
    controller_socket_removed: bool,
    sensitive_payloads_retained: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct IdentityEvidence {
    waldo_commit: String,
    verifier_commit: String,
    seed: u64,
    binaries: Vec<FileIdentity>,
    fixture: FileIdentity,
}

#[derive(Debug, Serialize)]
struct FileIdentity {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Serialize)]
struct CoordinateResult {
    event_coordinate_outcome: &'static str,
    delivered_pixel_provenance_outcome: &'static str,
    model_image_pixels: PixelSize,
    source_pixels: PixelSize,
    target_model_pixel: Point,
    target_logical_point: FloatPoint,
    expected_cell: Cell,
    observed_cell: Cell,
    event_error_pixels: FloatPoint,
    action_target_expected_rgb: [u8; 3],
    action_target_actual_rgba: [u8; 4],
    provenance_sample_model_pixel: Point,
    provenance_expected_rgb: [u8; 3],
    provenance_actual_rgba: [u8; 4],
    action_target_was_locally_obscured: bool,
}

#[derive(Debug, Serialize)]
struct DelayedCompletionResult {
    outcome: &'static str,
    product_stop_reason: Option<String>,
    application_outcome: Option<String>,
    frame_state: Option<String>,
    settle_deadline_reached: Option<bool>,
    click_reached_application: bool,
    returned_before_application_change: bool,
    returned_image_was_pre_change: bool,
    returned_image_was_post_change: bool,
    later_image_was_post_change: bool,
    application_changed_after_response: bool,
}

#[derive(Debug, Serialize)]
struct ContinuousCompletionResult {
    outcome: &'static str,
    product_stop_reason: Option<String>,
    application_outcome: Option<String>,
    frame_state: Option<String>,
    settle_deadline_reached: Option<bool>,
    external_deadline_reached: bool,
    frames_at_response: u64,
    frames_after_probe: u64,
    independent_oracle_still_changing: bool,
    later_image_digest_changed: Option<bool>,
    stop_completed_within_limit: bool,
}

#[derive(Debug, Serialize)]
struct TransparentInterceptionResult {
    outcome: &'static str,
    act_external_deadline_reached: bool,
    receipt_status: Option<String>,
    application_outcome: Option<String>,
    frame_state: Option<String>,
    settle_deadline_reached: Option<bool>,
    target_clicks: u64,
    interceptor_clicks: u64,
    stop_completed_within_limit: bool,
    service_healthy_after_stop: bool,
}

#[derive(Debug, Serialize)]
struct ContradictoryStateResult {
    outcome: &'static str,
    act_external_deadline_reached: bool,
    receipt_status: Option<String>,
    application_outcome: Option<String>,
    verification_results: Vec<bool>,
    verification_agreement: Option<String>,
    fixture_clicks: u64,
    fixture_phase: u64,
    stop_completed_within_limit: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct PixelSize {
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Point {
    x: u32,
    y: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct FloatPoint {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct Cell {
    column: u32,
    row: u32,
}

#[derive(Debug, Deserialize)]
struct FixtureState {
    phase: u64,
    click_count: u64,
    #[serde(default)]
    interceptor_count: u64,
    frame_count: u64,
    width: u32,
    height: u32,
    cells: Vec<FixtureCell>,
}

#[derive(Debug, Deserialize)]
struct FixtureCell {
    column: u32,
    row: u32,
    rgb: [u8; 3],
}

#[derive(Debug, Deserialize)]
struct FixtureEvent {
    kind: String,
    recorded_at_unix_ns: u128,
    x: f64,
    y: f64,
    column: u32,
    row: u32,
}

pub(crate) struct ToolObservation {
    pub(crate) id: u64,
    source: Value,
    source_pixels: PixelSize,
    model_pixels: PixelSize,
    logical_origin: FloatPoint,
    logical_width: f64,
    logical_height: f64,
    image: DynamicImage,
    image_digest: String,
    pub(crate) accessibility: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct AccessibilityFixtureState {
    title: String,
    invoke_count: u64,
    entry_text: String,
    entry_focused: bool,
    entry_generation: u64,
    spin_value: f64,
    spin_generation: u64,
    selected_label: Option<String>,
    duplicate_open: bool,
    control_sequence: u64,
}

struct ChildGuard(Child);

impl ChildGuard {
    fn terminate(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct ScratchDirectory {
    path: Option<PathBuf>,
}

impl ScratchDirectory {
    fn create(path: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir(&path)?;
        Ok(Self { path: Some(path) })
    }

    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("scratch directory exists until cleanup")
    }

    fn remove(mut self) -> Result<(), std::io::Error> {
        if let Some(path) = self.path.as_ref() {
            fs::remove_dir_all(path)?;
        }
        self.path = None;
        Ok(())
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

pub(crate) struct McpClient {
    child: ChildGuard,
    input: ChildStdin,
    output: mpsc::Receiver<ReaderMessage>,
    deferred: BTreeMap<u64, Value>,
    next_id: u64,
}

enum ReaderMessage {
    Response(Value),
    Error(String),
    Closed,
}

impl McpClient {
    fn start(binary: &Path, runtime: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut child = Command::new(binary)
            .env("XDG_RUNTIME_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or("waldo-mcp did not expose standard input")?;
        let output = child
            .stdout
            .take()
            .ok_or("waldo-mcp did not expose standard output")?;
        let (response_tx, response_rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let message = match line {
                    Ok(line) => match serde_json::from_str(&line) {
                        Ok(response) => ReaderMessage::Response(response),
                        Err(error) => ReaderMessage::Error(error.to_string()),
                    },
                    Err(error) => ReaderMessage::Error(error.to_string()),
                };
                if response_tx.send(message).is_err() {
                    return;
                }
            }
            let _ = response_tx.send(ReaderMessage::Closed);
        });
        let mut client = Self {
            child: ChildGuard(child),
            input,
            output: response_rx,
            deferred: BTreeMap::new(),
            next_id: 1,
        };
        client.call("initialize", json!({"protocolVersion": "2025-06-18"}))?;
        Ok(client)
    }

    pub(crate) fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        self.call_with_timeout(method, params, Duration::from_secs(10))?
            .ok_or_else(|| {
                format!("Waldo MCP {method} exceeded the 10-second verifier limit").into()
            })
    }

    pub(crate) fn call_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Option<Value>, Box<dyn std::error::Error>> {
        let id = self.send(method, params)?;
        self.receive(id, timeout)
    }

    pub(crate) fn send(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let id = self.next_id;
        self.next_id += 1;
        serde_json::to_writer(
            &mut self.input,
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;

        Ok(id)
    }

    pub(crate) fn receive(
        &mut self,
        id: u64,
        timeout: Duration,
    ) -> Result<Option<Value>, Box<dyn std::error::Error>> {
        if let Some(response) = self.deferred.remove(&id) {
            return Ok(Some(response));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            match self.output.recv_timeout(remaining) {
                Ok(ReaderMessage::Response(response)) => {
                    let response_id = response["id"]
                        .as_u64()
                        .ok_or("MCP response id is not a u64")?;
                    if response_id == id {
                        return Ok(Some(response));
                    }
                    self.deferred.insert(response_id, response);
                }
                Ok(ReaderMessage::Error(error)) => return Err(error.into()),
                Ok(ReaderMessage::Closed) => {
                    return Err("waldo-mcp closed standard output".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("waldo-mcp response reader stopped".into());
                }
            }
        }
    }
}

pub(crate) struct RuntimeHarness {
    scratch_path: PathBuf,
    scratch: ScratchDirectory,
    controller: ChildGuard,
    pub(crate) mcp: McpClient,
    identity: IdentityEvidence,
}

impl RuntimeHarness {
    pub(crate) fn start(options: &Options) -> Result<Self, Box<dyn std::error::Error>> {
        prepare_evidence_directory(&options.evidence_directory)?;
        let scratch_path = std::env::temp_dir().join(format!(
            "waldo-verification-{}-{}",
            std::process::id(),
            unix_ns()?
        ));
        let scratch = ScratchDirectory::create(scratch_path.clone())?;
        let runtime = scratch.path().join("runtime");
        let socket_directory = runtime.join("waldo");
        fs::create_dir_all(&socket_directory)?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(&socket_directory, fs::Permissions::from_mode(0o700))?;
        let socket = socket_directory.join("controller.sock");

        let controller = ChildGuard(
            Command::new(&options.waldod)
                .args(["--socket", socket.to_str().ok_or("non-UTF-8 socket path")?])
                .stderr(Stdio::inherit())
                .spawn()?,
        );
        wait_for_path(&socket, Duration::from_secs(5))?;
        let mcp = McpClient::start(&options.mcp, &runtime)?;
        let current_exe = std::env::current_exe()?;
        let identity = IdentityEvidence {
            waldo_commit: git_ref("main")?,
            verifier_commit: git_ref("HEAD")?,
            seed: options.seed,
            binaries: vec![
                file_identity(&options.waldod)?,
                file_identity(&options.mcp)?,
                file_identity(&current_exe)?,
            ],
            fixture: file_identity(&options.fixture)?,
        };
        write_json(options.evidence_directory.join("identity.json"), &identity)?;
        Ok(Self {
            scratch_path,
            scratch,
            controller,
            mcp,
            identity,
        })
    }

    pub(crate) fn shutdown(
        mut self,
    ) -> Result<(IdentityEvidence, bool), Box<dyn std::error::Error>> {
        self.mcp.child.terminate();
        self.controller.terminate();
        self.scratch.remove()?;
        Ok((self.identity, !self.scratch_path.exists()))
    }
}

pub fn run(options: Options) -> Result<OutsideInSummary, Box<dyn std::error::Error>> {
    let mut harness = RuntimeHarness::start(&options)?;
    let query = harness.mcp.call(
        "tools/call",
        json!({"name": "query", "arguments": {"include_windows": false}}),
    )?;
    write_json(
        options.evidence_directory.join("capability-snapshot.json"),
        &query,
    )?;

    let delayed = run_delayed(&options, &mut harness.mcp)?;
    let _ = harness
        .mcp
        .call("tools/call", json!({"name": "stop", "arguments": {}}));
    let continuous = run_continuous(&options, &mut harness.mcp)?;
    let _ = harness
        .mcp
        .call("tools/call", json!({"name": "stop", "arguments": {}}));
    let (transparent_interception, contradictory_state) = if continuous.outcome == "passed" {
        {
            let interception = run_transparent_interception(&options, &mut harness.mcp)?;
            let contradiction = if interception.service_healthy_after_stop {
                Some(run_contradictory_state(&options, &mut harness.mcp)?)
            } else {
                None
            };
            (Some(interception), contradiction)
        }
    } else {
        (None, None)
    };

    let output_id = delayed.output_id.clone();
    let (identity, controller_socket_removed) = harness.shutdown()?;

    let summary = OutsideInSummary {
        identity,
        output_id,
        coordinate: delayed.coordinate,
        delayed_completion: delayed.completion,
        continuous_completion: continuous,
        transparent_interception,
        contradictory_state,
        controller_socket_removed,
        sensitive_payloads_retained: false,
    };
    write_json(options.evidence_directory.join("summary.json"), &summary)?;
    Ok(summary)
}

pub fn run_accessibility(
    options: Options,
) -> Result<AccessibilitySummary, Box<dyn std::error::Error>> {
    let mut harness = RuntimeHarness::start(&options)?;
    let directory = options.evidence_directory.join("accessibility");
    fs::create_dir_all(&directory)?;
    let state_path = directory.join("fixture-state.json");
    let control_path = directory.join("fixture-control.json");
    write_json(
        control_path.clone(),
        &json!({"sequence": 0, "action": "none"}),
    )?;
    let mut fixture = start_accessibility_fixture(&options, &state_path, &control_path)?;
    let initial = wait_for_accessibility_state(&state_path, |_| true)?;
    thread::sleep(Duration::from_millis(500));
    let output_id = focused_output()?;
    write_json(
        directory.join("niri-topology.json"),
        &capture_niri_topology()?,
    )?;

    let fixture_title = initial.title.clone();
    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &fixture_title,
        &directory,
        "initial",
    )?;
    let (invoke_handle, invoke_action) =
        accessibility_action(&observed, "Invoke accessibility action")?;
    let invoke_response = accessibility_act(
        &mut harness.mcp,
        &observed,
        &invoke_handle,
        json!({"type": "invoke", "action": invoke_action}),
    )?;
    write_json(directory.join("invoke-response.json"), &invoke_response)?;
    let invoked = wait_for_accessibility_state(&state_path, |state| state.invoke_count == 1)?;

    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &fixture_title,
        &directory,
        "set-text",
    )?;
    let entry_handle = accessibility_handle(&observed, "Focus and SetText target")?;
    let set_text_response = accessibility_act(
        &mut harness.mcp,
        &observed,
        &entry_handle,
        json!({"type": "set_text", "text": "E2E016 exact text"}),
    )?;
    write_json(directory.join("set-text-response.json"), &set_text_response)?;
    let text_set =
        wait_for_accessibility_state(&state_path, |state| state.entry_text == "E2E016 exact text")?;

    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &fixture_title,
        &directory,
        "focus",
    )?;
    let entry_handle = accessibility_handle(&observed, "Focus and SetText target")?;
    let focus_response = accessibility_act(
        &mut harness.mcp,
        &observed,
        &entry_handle,
        json!({"type": "focus"}),
    )?;
    write_json(directory.join("focus-response.json"), &focus_response)?;
    let focused = read_accessibility_state(&state_path)?;
    let focus_error = first_receipt_error(&focus_response);
    let focus_supported =
        focused.entry_focused && first_receipt_status(&focus_response) == Some("executed".into());
    let focus_refused = focus_error.as_ref().is_some_and(|error| {
        error["code"] == "unsupported_capability" && error["retryable"] == false
    });
    let (focus_outcome, focus_truthful) = if focus_supported {
        ("supported_and_focused", true)
    } else if focus_refused && !focused.entry_focused {
        ("unsupported_before_mutation", true)
    } else {
        ("failed", false)
    };

    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &fixture_title,
        &directory,
        "set-value",
    )?;
    let value_handle = accessibility_handle(&observed, "SetValue target")?;
    write_json(
        control_path.clone(),
        &json!({"sequence": 1, "action": "refresh_value"}),
    )?;
    wait_for_accessibility_state(&state_path, |state| state.spin_generation == 1)?;
    thread::sleep(Duration::from_millis(200));
    let set_value_response = accessibility_act(
        &mut harness.mcp,
        &observed,
        &value_handle,
        json!({"type": "set_value", "value": "37"}),
    )?;
    write_json(
        directory.join("set-value-response.json"),
        &set_value_response,
    )?;
    let valued = wait_for_accessibility_state(&state_path, |state| {
        (state.spin_value - 37.0).abs() < f64::EPSILON
    })?;

    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &fixture_title,
        &directory,
        "select",
    )?;
    let selection_handle = accessibility_parent_handle(&observed, "Beta selection")?;
    let select_response = accessibility_act(
        &mut harness.mcp,
        &observed,
        &selection_handle,
        json!({"type": "select"}),
    )?;
    write_json(directory.join("select-response.json"), &select_response)?;
    let selected = wait_for_accessibility_state(&state_path, |state| {
        state.selected_label.as_deref() == Some("Beta selection")
    })?;

    let observed = observe_accessibility(
        &mut harness.mcp,
        &output_id,
        &fixture_title,
        &directory,
        "stale",
    )?;
    let stale_handle = accessibility_handle(&observed, "Focus and SetText target")?;
    write_json(
        control_path.clone(),
        &json!({"sequence": 2, "action": "recreate_entry"}),
    )?;
    let recreated = wait_for_accessibility_state(&state_path, |state| state.entry_generation == 1)?;
    let stale_response = accessibility_act(
        &mut harness.mcp,
        &observed,
        &stale_handle,
        json!({"type": "set_text", "text": "must not be written"}),
    )?;
    write_json(directory.join("stale-response.json"), &stale_response)?;
    let after_stale = read_accessibility_state(&state_path)?;
    let stale_replacement_refused = first_receipt_error(&stale_response)
        .is_some_and(|error| error["code"] == "stale_accessibility_target")
        && recreated.entry_text.is_empty()
        && after_stale.entry_text.is_empty();

    write_json(
        control_path,
        &json!({"sequence": 3, "action": "open_duplicate"}),
    )?;
    let duplicate = wait_for_accessibility_state(&state_path, |state| state.duplicate_open)?;
    thread::sleep(Duration::from_millis(500));
    let ambiguous_request = json!({
        "name": "observe",
        "arguments": {
            "source": {"type": "output", "output_id": output_id},
            "continue_from": null,
            "include_accessibility": true
        }
    });
    let ambiguous_response = harness.mcp.call("tools/call", ambiguous_request)?;
    write_json(
        directory.join("ambiguity-response.json"),
        &ambiguous_response,
    )?;
    let ambiguity_propagated = ambiguous_response["result"]["isError"] == true
        && ambiguous_response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("ambiguous_target:"));

    let _ = harness
        .mcp
        .call("tools/call", json!({"name": "stop", "arguments": {}}));
    fixture.terminate();
    let (identity, controller_socket_removed) = harness.shutdown()?;
    let summary = AccessibilitySummary {
        identity,
        invoke_reached_application: initial.invoke_count == 0 && invoked.invoke_count == 1,
        set_text_reached_application: text_set.entry_text == "E2E016 exact text",
        focus_outcome,
        focus_truthful,
        set_value_identity_refresh_reached_application: valued.spin_generation == 1
            && (valued.spin_value - 37.0).abs() < f64::EPSILON,
        selection_reached_application: selected.selected_label.as_deref() == Some("Beta selection"),
        stale_replacement_refused,
        ambiguity_propagated: duplicate.control_sequence == 3 && ambiguity_propagated,
        controller_socket_removed,
        sensitive_payloads_retained: false,
    };
    write_json(options.evidence_directory.join("summary.json"), &summary)?;
    Ok(summary)
}

struct DelayedRun {
    output_id: String,
    coordinate: CoordinateResult,
    completion: DelayedCompletionResult,
}

fn run_delayed(
    options: &Options,
    mcp: &mut McpClient,
) -> Result<DelayedRun, Box<dyn std::error::Error>> {
    let directory = options.evidence_directory.join("delayed");
    fs::create_dir_all(&directory)?;
    let state_path = directory.join("fixture-state.json");
    let events_path = directory.join("fixture-events.jsonl");
    let mut fixture = start_fixture(options, "delayed", &state_path, &events_path)?;
    let initial = wait_for_state(&state_path, |state| {
        state.phase == 0 && state.click_count == 0
    })?;
    // GTK's draw callback precedes the compositor presenting that buffer.
    thread::sleep(Duration::from_millis(500));
    let output_id = focused_output()?;
    write_json(
        directory.join("niri-topology.json"),
        &capture_niri_topology()?,
    )?;

    let observe_request = json!({
        "name": "observe",
        "arguments": {
            "source": {"type": "output", "output_id": output_id},
            "continue_from": null,
            "include_accessibility": false
        }
    });
    write_json(directory.join("observe-request.json"), &observe_request)?;
    let observed_response = mcp.call("tools/call", observe_request)?;
    let observed = parse_observation(&observed_response, &directory, "observe")?;
    write_redacted_response(
        directory.join("observe-response.json"),
        &observed_response,
        &observed,
    )?;

    let grid = TargetGrid::generate(
        FixtureRect {
            x: 0,
            y: 0,
            width: observed.model_pixels.width,
            height: observed.model_pixels.height,
        },
        5,
        3,
        options.seed,
    )?;
    write_json(directory.join("target-grid.json"), &grid)?;
    let (target, expected_cell) = select_target(&grid);
    let logical = model_to_logical(&observed, target.x, target.y);
    let expected_rgb = fixture_colour(&initial, expected_cell)?;
    let target_sample = observed.image.get_pixel(target.x, target.y).0;
    let provenance_sample = Point {
        x: (expected_cell.column * observed.model_pixels.width / 5)
            + (observed.model_pixels.width / 5 * 3 / 4),
        y: (expected_cell.row * observed.model_pixels.height / 3)
            + (observed.model_pixels.height / 3 * 3 / 4),
    };
    let provenance_rgba = observed
        .image
        .get_pixel(provenance_sample.x, provenance_sample.y)
        .0;
    let target_pixel_matches = rgb_near(target_sample, expected_rgb, 12);
    let delivered_pixel_matches_fixture = rgb_near(provenance_rgba, expected_rgb, 12);

    let act_request = json!({
        "name": "act",
        "arguments": {
            "observation_id": observed.id,
            "actions": [
                {
                    "type": "pointer_click",
                    "position": {"x": logical.x, "y": logical.y},
                    "button": "primary",
                    "count": 1
                },
                {
                    "type": "wait",
                    "predicate": {"type": "frame_changed", "from": observed.id},
                    "timeout_ms": 2000
                },
                {"type": "capture", "source": observed.source}
            ],
            "verification": []
        }
    });
    write_json(directory.join("act-request.json"), &act_request)?;
    let acted_response = mcp
        .call_with_timeout("tools/call", act_request, Duration::from_secs(10))?
        .ok_or("delayed completion Act exceeded the 10-second verifier limit")?;
    let response_ns = unix_ns()?;
    let acted = parse_observation(&acted_response, &directory, "act")?;
    write_redacted_response(directory.join("act-response.json"), &acted_response, &acted)?;
    let response_state = read_state(&state_path)?;
    let changed = wait_for_state(&state_path, |state| state.phase >= 1)?;
    let later_request = json!({
        "name": "observe",
        "arguments": {
            "source": observed.source,
            "continue_from": acted.id,
            "include_accessibility": false
        }
    });
    let later_response = mcp.call("tools/call", later_request)?;
    let later = parse_observation(&later_response, &directory, "later-observe")?;
    write_redacted_response(
        directory.join("later-observe-response.json"),
        &later_response,
        &later,
    )?;

    let release = wait_for_release(&events_path)?;
    let expected_x =
        f64::from(target.x) * f64::from(initial.width) / f64::from(observed.model_pixels.width);
    let expected_y =
        f64::from(target.y) * f64::from(initial.height) / f64::from(observed.model_pixels.height);
    let event_error = FloatPoint {
        x: (release.x - expected_x).abs(),
        y: (release.y - expected_y).abs(),
    };
    let observed_cell = Cell {
        column: release.column,
        row: release.row,
    };
    let event_coordinate_passed =
        observed_cell == expected_cell && event_error.x <= 2.0 && event_error.y <= 2.0;

    let pre_change_rgb = fixture_colour(&initial, Cell { column: 0, row: 0 })?;
    let post_change_rgb = fixture_colour(&changed, Cell { column: 0, row: 0 })?;
    let cell_centre = Point {
        x: observed.model_pixels.width / 10,
        y: observed.model_pixels.height / 6,
    };
    let returned_rgb = acted.image.get_pixel(cell_centre.x, cell_centre.y).0;
    let later_rgb = later.image.get_pixel(cell_centre.x, cell_centre.y).0;
    let returned_pre = rgb_near(returned_rgb, pre_change_rgb, 12);
    let returned_post = rgb_near(returned_rgb, post_change_rgb, 12);
    let later_post = rgb_near(later_rgb, post_change_rgb, 12);
    let click_reached_application = release.recorded_at_unix_ns <= response_ns;
    let changed_after_response = changed.phase >= 1 && response_state.phase == 0;
    let stop_reason = stop_reason(&acted_response);
    let application_outcome = result_text(&acted_response, "application_outcome");
    let frame_state = completion_text(&acted_response, "frame_state");
    let settle_deadline_reached = completion_bool(&acted_response, "settle_deadline_reached");
    let false_completion = stop_reason.as_deref() == Some("completed")
        && click_reached_application
        && returned_pre
        && later_post
        && changed_after_response;
    let truthful_completion = stop_reason.as_deref() == Some("completed")
        && click_reached_application
        && response_state.phase >= 1
        && returned_post
        && later_post;

    fixture.terminate();
    Ok(DelayedRun {
        output_id,
        coordinate: CoordinateResult {
            event_coordinate_outcome: if event_coordinate_passed {
                "passed"
            } else {
                "failed"
            },
            delivered_pixel_provenance_outcome: if delivered_pixel_matches_fixture {
                "passed"
            } else {
                "failed"
            },
            model_image_pixels: observed.model_pixels,
            source_pixels: observed.source_pixels,
            target_model_pixel: Point {
                x: target.x,
                y: target.y,
            },
            target_logical_point: logical,
            expected_cell,
            observed_cell,
            event_error_pixels: event_error,
            action_target_expected_rgb: expected_rgb,
            action_target_actual_rgba: target_sample,
            provenance_sample_model_pixel: provenance_sample,
            provenance_expected_rgb: expected_rgb,
            provenance_actual_rgba: provenance_rgba,
            action_target_was_locally_obscured: !target_pixel_matches
                && delivered_pixel_matches_fixture,
        },
        completion: DelayedCompletionResult {
            outcome: if false_completion {
                "failed_as_expected_on_baseline"
            } else if truthful_completion {
                "passed"
            } else {
                "inconclusive"
            },
            product_stop_reason: stop_reason,
            application_outcome,
            frame_state,
            settle_deadline_reached,
            click_reached_application,
            returned_before_application_change: response_state.phase == 0,
            returned_image_was_pre_change: returned_pre,
            returned_image_was_post_change: returned_post,
            later_image_was_post_change: later_post,
            application_changed_after_response: changed_after_response,
        },
    })
}

fn run_continuous(
    options: &Options,
    mcp: &mut McpClient,
) -> Result<ContinuousCompletionResult, Box<dyn std::error::Error>> {
    let directory = options.evidence_directory.join("continuous");
    fs::create_dir_all(&directory)?;
    let state_path = directory.join("fixture-state.json");
    let events_path = directory.join("fixture-events.jsonl");
    let mut fixture = start_fixture(options, "continuous", &state_path, &events_path)?;
    let initial = wait_for_state(&state_path, |state| state.click_count == 0)?;
    thread::sleep(Duration::from_millis(500));
    let output_id = focused_output()?;
    write_json(
        directory.join("niri-topology.json"),
        &capture_niri_topology()?,
    )?;
    let observe_request = json!({
        "name": "observe",
        "arguments": {
            "source": {"type": "output", "output_id": output_id},
            "continue_from": null,
            "include_accessibility": false
        }
    });
    let observed_response = mcp.call("tools/call", observe_request)?;
    let observed = parse_observation(&observed_response, &directory, "observe")?;
    write_redacted_response(
        directory.join("observe-response.json"),
        &observed_response,
        &observed,
    )?;
    let target = Point {
        x: observed.model_pixels.width / 2,
        y: observed.model_pixels.height / 2,
    };
    let logical = model_to_logical(&observed, target.x, target.y);
    let act_request = json!({
        "name": "act",
        "arguments": {
            "observation_id": observed.id,
            "actions": [{
                "type": "pointer_click",
                "position": {"x": logical.x, "y": logical.y},
                "button": "primary",
                "count": 1
            }],
            "verification": []
        }
    });
    write_json(directory.join("act-request.json"), &act_request)?;
    let acted_response =
        mcp.call_with_timeout("tools/call", act_request, Duration::from_secs(10))?;
    let Some(acted_response) = acted_response else {
        let at_response = wait_for_state(&state_path, |state| state.click_count >= 1)?;
        thread::sleep(Duration::from_millis(350));
        let after_probe = read_state(&state_path)?;
        let still_changing = initial.frame_count == 0
            && at_response.frame_count > 0
            && after_probe.frame_count > at_response.frame_count;
        write_json(
            directory.join("act-timeout.json"),
            &json!({
                "verifier_deadline_ms": 10000,
                "frames_at_deadline": at_response.frame_count,
                "frames_after_probe": after_probe.frame_count,
                "independent_oracle_still_changing": still_changing
            }),
        )?;
        let stop = mcp.call_with_timeout(
            "tools/call",
            json!({"name": "stop", "arguments": {}}),
            Duration::from_secs(5),
        )?;
        if let Some(response) = &stop {
            write_json(directory.join("stop-response.json"), response)?;
        }
        fixture.terminate();
        return Ok(ContinuousCompletionResult {
            outcome: if still_changing {
                "failed_as_expected_on_baseline"
            } else {
                "inconclusive"
            },
            product_stop_reason: None,
            application_outcome: None,
            frame_state: None,
            settle_deadline_reached: None,
            external_deadline_reached: true,
            frames_at_response: at_response.frame_count,
            frames_after_probe: after_probe.frame_count,
            independent_oracle_still_changing: still_changing,
            later_image_digest_changed: None,
            stop_completed_within_limit: stop.is_some(),
        });
    };
    let acted = parse_observation(&acted_response, &directory, "act")?;
    write_redacted_response(directory.join("act-response.json"), &acted_response, &acted)?;
    let at_response = wait_for_state(&state_path, |state| state.click_count >= 1)?;
    thread::sleep(Duration::from_millis(350));
    let after_probe = read_state(&state_path)?;
    let still_changing = initial.frame_count == 0
        && at_response.frame_count > 0
        && after_probe.frame_count > at_response.frame_count;
    let stop_reason = stop_reason(&acted_response);
    let application_outcome = result_text(&acted_response, "application_outcome");
    let frame_state = completion_text(&acted_response, "frame_state");
    let settle_deadline_reached = completion_bool(&acted_response, "settle_deadline_reached");
    let decisive_pass = application_outcome.as_deref() == Some("inconclusive")
        && frame_state.as_deref() == Some("changing")
        && settle_deadline_reached == Some(true)
        && still_changing;
    let stop = mcp.call_with_timeout(
        "tools/call",
        json!({"name": "stop", "arguments": {}}),
        Duration::from_secs(5),
    )?;
    if let Some(response) = &stop {
        write_json(directory.join("stop-response.json"), response)?;
    }
    fixture.terminate();

    Ok(ContinuousCompletionResult {
        outcome: if decisive_pass { "passed" } else { "failed" },
        product_stop_reason: stop_reason,
        application_outcome,
        frame_state,
        settle_deadline_reached,
        external_deadline_reached: false,
        frames_at_response: at_response.frame_count,
        frames_after_probe: after_probe.frame_count,
        independent_oracle_still_changing: still_changing,
        later_image_digest_changed: None,
        stop_completed_within_limit: stop.is_some(),
    })
}

fn run_transparent_interception(
    options: &Options,
    mcp: &mut McpClient,
) -> Result<TransparentInterceptionResult, Box<dyn std::error::Error>> {
    let directory = options.evidence_directory.join("transparent-interception");
    fs::create_dir_all(&directory)?;
    let state_path = directory.join("fixture-state.json");
    let events_path = directory.join("fixture-events.jsonl");
    let mut fixture = start_fixture(options, "interception", &state_path, &events_path)?;
    wait_for_state(&state_path, |state| {
        state.click_count == 0 && state.interceptor_count == 0
    })?;
    thread::sleep(Duration::from_millis(500));
    let output_id = focused_output()?;
    write_json(
        directory.join("niri-topology.json"),
        &capture_niri_topology()?,
    )?;
    let observe_request = json!({
        "name": "observe",
        "arguments": {
            "source": {"type": "output", "output_id": output_id},
            "continue_from": null,
            "include_accessibility": false
        }
    });
    write_json(directory.join("observe-request.json"), &observe_request)?;
    let observed_response = mcp.call("tools/call", observe_request)?;
    let observed = parse_observation(&observed_response, &directory, "observe")?;
    write_redacted_response(
        directory.join("observe-response.json"),
        &observed_response,
        &observed,
    )?;
    let target = Point {
        x: observed.model_pixels.width / 2,
        y: observed.model_pixels.height / 2,
    };
    let logical = model_to_logical(&observed, target.x, target.y);
    let act_request = json!({
        "name": "act",
        "arguments": {
            "observation_id": observed.id,
            "actions": [{
                "type": "pointer_click",
                "position": {"x": logical.x, "y": logical.y},
                "button": "primary",
                "count": 1
            }],
            "verification": []
        }
    });
    write_json(directory.join("act-request.json"), &act_request)?;
    let acted_response =
        mcp.call_with_timeout("tools/call", act_request, Duration::from_secs(10))?;
    let Some(acted_response) = acted_response else {
        let state = wait_for_state(&state_path, |state| state.interceptor_count >= 1)?;
        write_json(
            directory.join("act-timeout.json"),
            &json!({
                "verifier_deadline_ms": 10000,
                "target_clicks": state.click_count,
                "interceptor_clicks": state.interceptor_count,
                "classification": "blocked_inconclusive"
            }),
        )?;
        let stop = stop_and_record(mcp, &directory)?;
        let healthy = service_health_check(mcp, &directory)?;
        fixture.terminate();
        return Ok(TransparentInterceptionResult {
            outcome: "blocked_inconclusive",
            act_external_deadline_reached: true,
            receipt_status: None,
            application_outcome: None,
            frame_state: None,
            settle_deadline_reached: None,
            target_clicks: state.click_count,
            interceptor_clicks: state.interceptor_count,
            stop_completed_within_limit: stop,
            service_healthy_after_stop: healthy,
        });
    };
    let acted = parse_observation(&acted_response, &directory, "act")?;
    write_redacted_response(directory.join("act-response.json"), &acted_response, &acted)?;
    let state = wait_for_state(&state_path, |state| state.interceptor_count >= 1)?;
    let receipt_status = first_receipt_status(&acted_response);
    let application_outcome = result_text(&acted_response, "application_outcome");
    let frame_state = completion_text(&acted_response, "frame_state");
    let settle_deadline_reached = completion_bool(&acted_response, "settle_deadline_reached");
    let passed = receipt_status.as_deref() == Some("executed")
        && application_outcome.as_deref() == Some("inconclusive")
        && state.click_count == 0
        && state.interceptor_count == 1;
    let stop = stop_and_record(mcp, &directory)?;
    let healthy = service_health_check(mcp, &directory)?;
    fixture.terminate();

    Ok(TransparentInterceptionResult {
        outcome: if passed { "passed" } else { "failed" },
        act_external_deadline_reached: false,
        receipt_status,
        application_outcome,
        frame_state,
        settle_deadline_reached,
        target_clicks: state.click_count,
        interceptor_clicks: state.interceptor_count,
        stop_completed_within_limit: stop,
        service_healthy_after_stop: healthy,
    })
}

fn run_contradictory_state(
    options: &Options,
    mcp: &mut McpClient,
) -> Result<ContradictoryStateResult, Box<dyn std::error::Error>> {
    let directory = options.evidence_directory.join("contradictory-state");
    fs::create_dir_all(&directory)?;
    let state_path = directory.join("fixture-state.json");
    let events_path = directory.join("fixture-events.jsonl");
    let mut fixture = start_fixture(options, "contradictory", &state_path, &events_path)?;
    let fixture_pid = fixture.0.id();
    wait_for_state(&state_path, |state| state.click_count == 0)?;
    thread::sleep(Duration::from_millis(500));
    let output_id = focused_output()?;
    let window_id = focused_window_id()?;
    write_json(
        directory.join("niri-topology.json"),
        &capture_niri_topology()?,
    )?;
    let observe_request = json!({
        "name": "observe",
        "arguments": {
            "source": {"type": "output", "output_id": output_id},
            "continue_from": null,
            "include_accessibility": false
        }
    });
    write_json(directory.join("observe-request.json"), &observe_request)?;
    let observed_response = mcp.call("tools/call", observe_request)?;
    let observed = parse_observation(&observed_response, &directory, "observe")?;
    write_redacted_response(
        directory.join("observe-response.json"),
        &observed_response,
        &observed,
    )?;
    let target = Point {
        x: observed.model_pixels.width / 2,
        y: observed.model_pixels.height / 2,
    };
    let logical = model_to_logical(&observed, target.x, target.y);
    let act_request = json!({
        "name": "act",
        "arguments": {
            "observation_id": observed.id,
            "actions": [{
                "type": "pointer_click",
                "position": {"x": logical.x, "y": logical.y},
                "button": "primary",
                "count": 1
            }],
            "verification": [
                {"type": "focused_window", "window_id": window_id},
                {"type": "process_exited", "pid": fixture_pid}
            ]
        }
    });
    write_json(directory.join("act-request.json"), &act_request)?;
    let acted_response =
        mcp.call_with_timeout("tools/call", act_request, Duration::from_secs(10))?;
    let Some(acted_response) = acted_response else {
        let state = read_state(&state_path)?;
        write_json(
            directory.join("act-timeout.json"),
            &json!({
                "verifier_deadline_ms": 10000,
                "fixture_clicks": state.click_count,
                "fixture_phase": state.phase,
                "classification": "blocked_inconclusive"
            }),
        )?;
        let stop = stop_and_record(mcp, &directory)?;
        fixture.terminate();
        return Ok(ContradictoryStateResult {
            outcome: "blocked_inconclusive",
            act_external_deadline_reached: true,
            receipt_status: None,
            application_outcome: None,
            verification_results: Vec::new(),
            verification_agreement: None,
            fixture_clicks: state.click_count,
            fixture_phase: state.phase,
            stop_completed_within_limit: stop,
        });
    };
    let acted = parse_observation(&acted_response, &directory, "act")?;
    write_redacted_response(directory.join("act-response.json"), &acted_response, &acted)?;
    let state = wait_for_state(&state_path, |state| {
        state.click_count >= 1 && state.phase >= 1
    })?;
    let receipt_status = first_receipt_status(&acted_response);
    let application_outcome = result_text(&acted_response, "application_outcome");
    let verification_results =
        acted_response["result"]["structuredContent"]["result"]["verification"]
            .as_array()
            .map(|results| results.iter().filter_map(Value::as_bool).collect())
            .unwrap_or_default();
    let verification_agreement = completion_text(&acted_response, "verification_agreement");
    let passed = receipt_status.as_deref() == Some("executed")
        && application_outcome.as_deref() == Some("inconclusive")
        && verification_results == [true, false]
        && verification_agreement.as_deref() == Some("contradictory")
        && state.click_count == 1
        && state.phase >= 1;
    let stop = stop_and_record(mcp, &directory)?;
    fixture.terminate();

    Ok(ContradictoryStateResult {
        outcome: if passed { "passed" } else { "failed" },
        act_external_deadline_reached: false,
        receipt_status,
        application_outcome,
        verification_results,
        verification_agreement,
        fixture_clicks: state.click_count,
        fixture_phase: state.phase,
        stop_completed_within_limit: stop,
    })
}

fn stop_and_record(
    mcp: &mut McpClient,
    directory: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let stop = mcp.call_with_timeout(
        "tools/call",
        json!({"name": "stop", "arguments": {}}),
        Duration::from_secs(5),
    )?;
    if let Some(response) = &stop {
        write_json(directory.join("stop-response.json"), response)?;
    }
    Ok(stop.is_some())
}

fn service_health_check(
    mcp: &mut McpClient,
    directory: &Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let response = mcp.call_with_timeout(
        "tools/call",
        json!({"name": "query", "arguments": {"include_windows": false}}),
        Duration::from_secs(5),
    )?;
    if let Some(response) = &response {
        write_json(directory.join("post-stop-health.json"), response)?;
    }
    Ok(response
        .as_ref()
        .is_some_and(|response| response["result"]["isError"].as_bool() != Some(true)))
}

fn start_fixture(
    options: &Options,
    scenario: &str,
    state: &Path,
    events: &Path,
) -> Result<ChildGuard, Box<dyn std::error::Error>> {
    Ok(ChildGuard(
        Command::new(&options.python)
            .arg(&options.fixture)
            .args(["--events", events.to_str().ok_or("non-UTF-8 events path")?])
            .args(["--ready", state.to_str().ok_or("non-UTF-8 state path")?])
            .args(["--seed", &options.seed.to_string()])
            .args(["--scenario", scenario])
            .args(
                (scenario == "delayed")
                    .then_some(["--delay-ms", "2500"])
                    .into_iter()
                    .flatten(),
            )
            .stderr(Stdio::inherit())
            .spawn()?,
    ))
}

fn start_accessibility_fixture(
    options: &Options,
    state: &Path,
    control: &Path,
) -> Result<ChildGuard, Box<dyn std::error::Error>> {
    Ok(ChildGuard(
        Command::new(&options.python)
            .arg(&options.fixture)
            .args(["--state", state.to_str().ok_or("non-UTF-8 state path")?])
            .args([
                "--control",
                control.to_str().ok_or("non-UTF-8 control path")?,
            ])
            .args(["--seed", &options.seed.to_string()])
            .stderr(Stdio::inherit())
            .spawn()?,
    ))
}

pub(crate) fn observe_accessibility(
    mcp: &mut McpClient,
    output_id: &str,
    fixture_title: &str,
    directory: &Path,
    label: &str,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    observe_output(mcp, output_id, fixture_title, directory, label, true)
}

pub(crate) fn observe_without_accessibility(
    mcp: &mut McpClient,
    output_id: &str,
    fixture_title: &str,
    directory: &Path,
    label: &str,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    observe_output(mcp, output_id, fixture_title, directory, label, false)
}

pub(crate) fn observe_accessibility_transient(
    mcp: &mut McpClient,
    output_id: &str,
    fixture_title: &str,
    private_directory: &Path,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    focus_niri_window(fixture_title)?;
    thread::sleep(Duration::from_millis(100));
    let response = mcp
        .call_with_timeout(
            "tools/call",
            json!({
                "name": "observe",
                "arguments": {
                    "source": {"type": "output", "output_id": output_id},
                    "continue_from": null,
                    "include_accessibility": true
                }
            }),
            Duration::from_secs(30),
        )?
        .ok_or("transient accessibility Observe exceeded the 30-second verifier limit")?;
    parse_observation(&response, private_directory, "transient-accessibility")
}

fn observe_output(
    mcp: &mut McpClient,
    output_id: &str,
    fixture_title: &str,
    directory: &Path,
    label: &str,
    include_accessibility: bool,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    focus_niri_window(fixture_title)?;
    thread::sleep(Duration::from_millis(100));
    let request = json!({
        "name": "observe",
        "arguments": {
            "source": {"type": "output", "output_id": output_id},
            "continue_from": null,
            "include_accessibility": include_accessibility
        }
    });
    write_json(
        directory.join(format!("{label}-observe-request.json")),
        &request,
    )?;
    let response = mcp
        .call_with_timeout("tools/call", request, Duration::from_secs(30))?
        .ok_or({
            if include_accessibility {
                "accessibility Observe exceeded the 30-second verifier limit"
            } else {
                "Observe exceeded the 30-second verifier limit"
            }
        })?;
    let observation = parse_observation(&response, directory, &format!("{label}-observe"))?;
    write_redacted_response(
        directory.join(format!("{label}-observe-response.json")),
        &response,
        &observation,
    )?;
    Ok(observation)
}

pub(crate) fn focus_niri_window(title: &str) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let window_id = loop {
        let mut socket = Socket::connect()?;
        let reply = socket.send(Request::Windows)??;
        let Response::Windows(windows) = reply else {
            return Err("Niri returned an unexpected response to Windows".into());
        };
        let matches: Vec<_> = windows
            .iter()
            .filter(|window| window.title.as_deref() == Some(title))
            .collect();
        match matches.as_slice() {
            [window] => break window.id,
            [] if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => {
                return Err(format!(
                    "expected one Niri fixture window titled {title:?}, found {}",
                    matches.len()
                )
                .into());
            }
        }
    };
    let mut socket = Socket::connect()?;
    let reply = socket.send(Request::Action(NiriAction::FocusWindow { id: window_id }))??;
    if !matches!(reply, Response::Handled) {
        return Err("Niri did not acknowledge fixture focus".into());
    }
    Ok(())
}

pub(crate) fn accessibility_handle(
    observation: &ToolObservation,
    name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let matches: Vec<_> = observation
        .accessibility
        .iter()
        .filter(|node| node["name"].as_str() == Some(name))
        .collect();
    let [node] = matches.as_slice() else {
        return Err(format!(
            "expected one accessibility node named {name:?}, found {}",
            matches.len()
        )
        .into());
    };
    node["handle"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("accessibility node {name:?} has no string handle").into())
}

pub(crate) fn accessibility_handle_with_role(
    observation: &ToolObservation,
    name: &str,
    role: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let matches: Vec<_> = observation
        .accessibility
        .iter()
        .filter(|node| node["name"].as_str() == Some(name))
        .filter(|node| node["role"].as_str() == Some(role))
        .collect();
    let [node] = matches.as_slice() else {
        return Err(format!(
            "expected one accessibility {role} named {name:?}, found {}",
            matches.len()
        )
        .into());
    };
    node["handle"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("accessibility {role} {name:?} has no string handle").into())
}

pub(crate) fn accessibility_action(
    observation: &ToolObservation,
    name: &str,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let matches: Vec<_> = observation
        .accessibility
        .iter()
        .filter(|node| node["name"].as_str() == Some(name))
        .filter_map(|node| {
            let action = node["actions"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .find(|action| action.eq_ignore_ascii_case("click"))?;
            Some((node["handle"].as_str()?.to_owned(), action.to_owned()))
        })
        .collect();
    let [selected] = matches.as_slice() else {
        return Err(format!(
            "expected one clickable accessibility node named {name:?}, found {}",
            matches.len()
        )
        .into());
    };
    Ok(selected.clone())
}

fn accessibility_parent_handle(
    observation: &ToolObservation,
    child_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let child = accessibility_handle(observation, child_name)?;
    let matches: Vec<_> = observation
        .accessibility
        .iter()
        .filter(|node| {
            node["children"]
                .as_array()
                .is_some_and(|children| children.iter().any(|handle| handle == &child))
        })
        .collect();
    let [parent] = matches.as_slice() else {
        return Err(format!(
            "expected one accessibility parent for {child_name:?}, found {}",
            matches.len()
        )
        .into());
    };
    parent["handle"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("accessibility parent for {child_name:?} has no handle").into())
}

fn accessibility_act(
    mcp: &mut McpClient,
    observation: &ToolObservation,
    element: &str,
    operation: Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    mcp.call_with_timeout(
        "tools/call",
        json!({
            "name": "act",
            "arguments": {
                "observation_id": observation.id,
                "actions": [{
                    "type": "accessibility",
                    "element": element,
                    "operation": operation
                }],
                "verification": []
            }
        }),
        Duration::from_secs(30),
    )?
    .ok_or_else(|| "accessibility Act exceeded the 30-second verifier limit".into())
}

fn first_receipt_error(response: &Value) -> Option<Value> {
    response["result"]["structuredContent"]["result"]["receipts"]
        .as_array()?
        .first()?
        .get("error")
        .cloned()
}

fn read_accessibility_state(
    path: &Path,
) -> Result<AccessibilityFixtureState, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn wait_for_accessibility_state(
    path: &Path,
    predicate: impl Fn(&AccessibilityFixtureState) -> bool,
) -> Result<AccessibilityFixtureState, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(state) = read_accessibility_state(path)
            && predicate(&state)
        {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for accessibility fixture at {}",
                path.display()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn parse_observation(
    response: &Value,
    directory: &Path,
    label: &str,
) -> Result<ToolObservation, Box<dyn std::error::Error>> {
    if response["result"]["isError"].as_bool() == Some(true) {
        return Err(format!("Waldo MCP {label} failed: {}", response["result"]).into());
    }
    let structured = &response["result"]["structuredContent"];
    let observation = structured
        .get("observation")
        .or_else(|| structured.get("result")?.get("observation"))
        .ok_or_else(|| format!("MCP {label} response has no observation"))?;
    let image_data = response["result"]["content"]
        .as_array()
        .and_then(|content| content.iter().find(|item| item["type"] == "image"))
        .and_then(|item| item["data"].as_str())
        .ok_or_else(|| format!("MCP {label} response has no image content"))?;
    let image_bytes = BASE64.decode(image_data)?;
    let image = image::load_from_memory_with_format(&image_bytes, ImageFormat::Png)?;
    let image_digest = sha256_bytes(&image_bytes)?;
    let source_pixels: PixelSize =
        serde_json::from_value(observation["coordinates"]["source_pixels"].clone())?;
    let model_pixels: PixelSize =
        serde_json::from_value(observation["coordinates"]["model_image_pixels"].clone())?;
    if image.dimensions() != (model_pixels.width, model_pixels.height) {
        return Err(
            format!("delivered {label} PNG dimensions do not match model_image_pixels").into(),
        );
    }
    write_json(
        directory.join(format!("{label}-image-evidence.json")),
        &json!({
            "mime_type": "image/png",
            "byte_length": image_bytes.len(),
            "sha256": image_digest,
            "width": model_pixels.width,
            "height": model_pixels.height,
            "payload_retained": false
        }),
    )?;
    Ok(ToolObservation {
        id: observation["id"]
            .as_u64()
            .ok_or("observation id is not a u64")?,
        source: observation["source"].clone(),
        source_pixels,
        model_pixels,
        logical_origin: FloatPoint {
            x: observation["coordinates"]["logical_bounds"]["origin"]["x"]
                .as_f64()
                .ok_or("logical origin x is not numeric")?,
            y: observation["coordinates"]["logical_bounds"]["origin"]["y"]
                .as_f64()
                .ok_or("logical origin y is not numeric")?,
        },
        logical_width: observation["coordinates"]["logical_bounds"]["width"]
            .as_f64()
            .ok_or("logical width is not numeric")?,
        logical_height: observation["coordinates"]["logical_bounds"]["height"]
            .as_f64()
            .ok_or("logical height is not numeric")?,
        image,
        image_digest,
        accessibility: observation["accessibility"]
            .as_array()
            .cloned()
            .ok_or("observation accessibility is not an array")?,
    })
}

fn write_redacted_response(
    path: PathBuf,
    response: &Value,
    observation: &ToolObservation,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut redacted = response.clone();
    if let Some(content) = redacted["result"]["content"].as_array_mut() {
        for item in content {
            if item["type"] == "image" {
                let object = item
                    .as_object_mut()
                    .ok_or("MCP image content was not an object")?;
                object.remove("data");
                object.insert("payload_retained".into(), Value::Bool(false));
                object.insert(
                    "sha256".into(),
                    Value::String(observation.image_digest.clone()),
                );
            }
        }
    }
    write_json(path, &redacted)
}

fn select_target(grid: &TargetGrid) -> (&waldo_verification::FixturePoint, Cell) {
    grid.targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let column = index as u32 % grid.columns;
            let row = index as u32 / grid.columns;
            let cell_width = grid.bounds.width / grid.columns;
            let cell_height = grid.bounds.height / grid.rows;
            let local_x = target.x - column * cell_width;
            let local_y = target.y - row * cell_height;
            let margin = local_x
                .min(cell_width.saturating_sub(local_x + 1))
                .min(local_y)
                .min(cell_height.saturating_sub(local_y + 1));
            (margin, target, Cell { column, row })
        })
        .max_by_key(|(margin, _, _)| *margin)
        .map(|(_, target, cell)| (target, cell))
        .expect("target grids are non-empty")
}

fn model_to_logical(observation: &ToolObservation, x: u32, y: u32) -> FloatPoint {
    FloatPoint {
        x: observation.logical_origin.x
            + f64::from(x) * observation.logical_width / f64::from(observation.model_pixels.width),
        y: observation.logical_origin.y
            + f64::from(y) * observation.logical_height
                / f64::from(observation.model_pixels.height),
    }
}

fn fixture_colour(state: &FixtureState, cell: Cell) -> Result<[u8; 3], Box<dyn std::error::Error>> {
    state
        .cells
        .iter()
        .find(|candidate| candidate.column == cell.column && candidate.row == cell.row)
        .map(|candidate| candidate.rgb)
        .ok_or_else(|| format!("fixture state omitted cell {},{}", cell.column, cell.row).into())
}

fn rgb_near(actual: [u8; 4], expected: [u8; 3], tolerance: u8) -> bool {
    actual[..3]
        .iter()
        .zip(expected)
        .all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
}

fn stop_reason(response: &Value) -> Option<String> {
    result_text(response, "stop_reason")
}

fn result_text(response: &Value, field: &str) -> Option<String> {
    response["result"]["structuredContent"]["result"][field]
        .as_str()
        .map(str::to_owned)
}

fn first_receipt_status(response: &Value) -> Option<String> {
    response["result"]["structuredContent"]["result"]["receipts"][0]["status"]
        .as_str()
        .map(str::to_owned)
}

fn completion_text(response: &Value, field: &str) -> Option<String> {
    response["result"]["structuredContent"]["result"]["completion_evidence"][field]
        .as_str()
        .map(str::to_owned)
}

fn completion_bool(response: &Value, field: &str) -> Option<bool> {
    response["result"]["structuredContent"]["result"]["completion_evidence"][field].as_bool()
}

pub(crate) fn focused_output() -> Result<String, Box<dyn std::error::Error>> {
    let response = Socket::connect()?
        .send(Request::FocusedOutput)?
        .map_err(std::io::Error::other)?;
    let Response::FocusedOutput(Some(output)) = response else {
        return Err("Niri has no focused output".into());
    };
    Ok(output.name)
}

fn focused_window_id() -> Result<String, Box<dyn std::error::Error>> {
    let response = Socket::connect()?
        .send(Request::Windows)?
        .map_err(std::io::Error::other)?;
    let Response::Windows(windows) = response else {
        return Err("Niri returned an unexpected response to Windows".into());
    };
    windows
        .into_iter()
        .find(|window| window.is_focused)
        .map(|window| window.id.to_string())
        .ok_or_else(|| "Niri has no focused window".into())
}

fn prepare_evidence_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!(
            "retained evidence directory cannot be a symbolic link: {}",
            path.display()
        )
        .into());
    }
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(format!(
            "retained evidence path is not a directory: {}",
            path.display()
        )
        .into());
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o700 {
        return Err(format!(
            "retained evidence directory {} has mode {mode:o}, expected 700",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("timed out waiting for {}", path.display()).into())
}

fn wait_for_state(
    path: &Path,
    predicate: impl Fn(&FixtureState) -> bool,
) -> Result<FixtureState, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(state) = read_state(path)
            && predicate(&state)
        {
            return Ok(state);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(format!("timed out waiting for fixture state at {}", path.display()).into())
}

fn read_state(path: &Path) -> Result<FixtureState, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn wait_for_release(path: &Path) -> Result<FixtureEvent, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(path) {
            if let Some(event) = release_from_complete_jsonl(&contents)? {
                return Ok(event);
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err("timed out waiting for the fixture release event".into())
}

fn release_from_complete_jsonl(contents: &str) -> Result<Option<FixtureEvent>, serde_json::Error> {
    for framed_line in contents.split_inclusive('\n') {
        if !framed_line.ends_with('\n') {
            continue;
        }
        let line = framed_line
            .strip_suffix('\n')
            .expect("completed JSONL line ends with newline");
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            continue;
        }
        let event: FixtureEvent = serde_json::from_str(line)?;
        if event.kind == "target_release" {
            return Ok(Some(event));
        }
    }
    Ok(None)
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("sha256sum did not expose standard input")?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("sha256sum failed".into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(stdout
        .split_whitespace()
        .next()
        .ok_or("sha256sum returned no digest")?
        .to_owned())
}

fn file_identity(path: &Path) -> Result<FileIdentity, Box<dyn std::error::Error>> {
    let output = Command::new("sha256sum").arg(path).output()?;
    if !output.status.success() {
        return Err(format!("sha256sum failed for {}", path.display()).into());
    }
    let stdout = String::from_utf8(output.stdout)?;
    Ok(FileIdentity {
        path: path.to_owned(),
        sha256: stdout
            .split_whitespace()
            .next()
            .ok_or("sha256sum returned no digest")?
            .to_owned(),
    })
}

fn git_ref(reference: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(["rev-parse", reference])
        .output()?;
    if !output.status.success() {
        return Err(format!("git rev-parse {reference} failed").into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

pub(crate) fn write_json(
    path: PathBuf,
    value: &impl Serialize,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes)?;
    Ok(())
}

fn unix_ns() -> Result<u128, std::time::SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        thread,
        time::Duration,
    };

    use image::{DynamicImage, RgbaImage};
    use waldo_verification::{FixtureRect, TargetGrid};

    use super::{
        FloatPoint, PixelSize, ScratchDirectory, ToolObservation, accessibility_handle_with_role,
        model_to_logical, prepare_evidence_directory, release_from_complete_jsonl, rgb_near,
        select_target, unix_ns, wait_for_release,
    };

    fn temporary_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waldo-verification-test-{label}-{}-{}",
            std::process::id(),
            unix_ns().unwrap()
        ))
    }

    #[test]
    fn selected_seeded_target_stays_well_inside_its_expected_cell() {
        let grid = TargetGrid::generate(
            FixtureRect {
                x: 0,
                y: 0,
                width: 2048,
                height: 1152,
            },
            5,
            3,
            0x9c2e,
        )
        .unwrap();
        let (target, cell) = select_target(&grid);

        assert_eq!(cell.column, target.x * 5 / 2048);
        assert_eq!(cell.row, target.y * 3 / 1152);
    }

    #[test]
    fn delivered_model_pixels_map_to_the_published_logical_rectangle() {
        let observation = ToolObservation {
            id: 1,
            source: serde_json::Value::Null,
            source_pixels: PixelSize {
                width: 2560,
                height: 1440,
            },
            model_pixels: PixelSize {
                width: 2048,
                height: 1152,
            },
            logical_origin: FloatPoint { x: 2560.0, y: 0.0 },
            logical_width: 2560.0,
            logical_height: 1440.0,
            image: DynamicImage::ImageRgba8(RgbaImage::new(1, 1)),
            image_digest: String::new(),
            accessibility: Vec::new(),
        };

        let logical = model_to_logical(&observation, 1024, 576);

        assert_eq!(logical.x, 3840.0);
        assert_eq!(logical.y, 720.0);
    }

    #[test]
    fn role_qualified_status_lookup_ignores_the_duplicate_label_node() {
        let observation = ToolObservation {
            id: 1,
            source: serde_json::Value::Null,
            source_pixels: PixelSize {
                width: 1,
                height: 1,
            },
            model_pixels: PixelSize {
                width: 1,
                height: 1,
            },
            logical_origin: FloatPoint { x: 0.0, y: 0.0 },
            logical_width: 1.0,
            logical_height: 1.0,
            image: DynamicImage::ImageRgba8(RgbaImage::new(1, 1)),
            image_digest: String::new(),
            accessibility: vec![
                serde_json::json!({
                    "handle": "element-9",
                    "name": "Destination digest matched",
                    "role": "checkbox",
                    "states": ["checkable"]
                }),
                serde_json::json!({
                    "handle": "element-13",
                    "name": "Destination digest matched",
                    "role": "label",
                    "states": ["visible"]
                }),
            ],
        };

        assert_eq!(
            accessibility_handle_with_role(&observation, "Destination digest matched", "checkbox")
                .unwrap(),
            "element-9"
        );
    }

    #[test]
    fn fixture_pixel_comparison_allows_only_small_capture_variance() {
        assert!(rgb_near([100, 101, 102, 255], [98, 100, 104], 3));
        assert!(!rgb_near([100, 101, 120, 255], [98, 100, 104], 3));
    }

    #[test]
    fn release_wait_ignores_a_partial_tail_but_rejects_a_malformed_complete_line() {
        let path = temporary_path("partial-jsonl");
        fs::write(
            &path,
            r#"{"kind":"target_release","recorded_at_unix_ns":1,"x":2.0,"y":3.0,"column":0,"#,
        )
        .unwrap();
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut file = OpenOptions::new().append(true).open(writer_path).unwrap();
            file.write_all(b"\"row\":0}\n").unwrap();
        });

        let event = wait_for_release(&path).unwrap();

        writer.join().unwrap();
        assert_eq!(event.kind, "target_release");
        assert_eq!(event.x, 2.0);
        fs::remove_file(path).unwrap();
        assert!(release_from_complete_jsonl("{not-json}").unwrap().is_none());
        assert!(release_from_complete_jsonl("{not-json}\n").is_err());
    }

    #[test]
    fn scratch_directory_is_removed_during_an_early_error_return() {
        fn fail_after_create(path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
            let scratch = ScratchDirectory::create(path)?;
            fs::write(scratch.path().join("marker"), b"temporary")?;
            Err("forced verifier error".into())
        }

        let path = temporary_path("scratch-cleanup");
        assert!(fail_after_create(path.clone()).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn retained_evidence_directory_is_corrected_to_owner_only_permissions() {
        let path = temporary_path("evidence-mode");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

        prepare_evidence_directory(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir(path).unwrap();
    }
}
