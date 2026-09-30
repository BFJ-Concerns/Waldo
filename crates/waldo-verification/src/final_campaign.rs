use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use niri_ipc::{Action as NiriAction, Request, Response, socket::Socket};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use waldo_verification::{
    AdmissionOutcome, CONFORMANCE_SCHEMA_VERSION, CampaignCell, CampaignOutcome, DisplayRoute,
    EvidenceMaturity, FinalCampaignRecord, FixtureState, FixtureToolkit, PrivacyScan, PrivateRoot,
    complete_fixture_events, scan_private_root,
};

use crate::{
    clipboard_outside_in,
    outside_in::{
        self, ChildGuard, RuntimeHarness, ToolObservation, focus_niri_window,
        observe_window_without_accessibility, parse_observation, sha256_bytes, write_json,
        write_redacted_response,
    },
    toolkit_preflight,
};

pub struct Options {
    pub evidence_directory: PathBuf,
    pub waldod: PathBuf,
    pub mcp: PathBuf,
    pub cli: PathBuf,
    pub fixture_directory: PathBuf,
    pub corpus_directory: PathBuf,
    pub plugin_manifest: PathBuf,
    pub python: PathBuf,
    pub electron: PathBuf,
    pub seed: u64,
    pub skip_legacy: bool,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCampaignSummary {
    evidence_maturity: EvidenceMaturity,
    binding_shapes_settled: bool,
    corpus_schema_major: u16,
    source_commit: String,
    source_tree_clean: bool,
    coverage: FinalCampaignRecord,
    coverage_totals: CoverageTotals,
    coherent_act: Option<CoherentActEvidence>,
    controller_runtime_removed: bool,
    fixture_processes_removed: bool,
    privacy_scan: PrivacyScan,
    sensitive_payloads_retained: bool,
    failed: bool,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct CoverageTotals {
    passed: usize,
    failed: usize,
    inconclusive: usize,
    unavailable: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct CoherentActEvidence {
    saved_document: String,
    saved_document_sha256: String,
    pointer_move_distinctive: bool,
    drag_distinctive: bool,
    click_distinctive: bool,
    text_distinctive: bool,
    wait_process_exited: bool,
    capture_returned: bool,
    screenshot_changed: bool,
    ordered_receipts: bool,
    application_event_count: usize,
    final_image_sha256: String,
    fixture_process_removed: bool,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct CampaignIdentity {
    source_commit: String,
    source_tree_clean: bool,
    seed: u64,
    files: Vec<FileIdentity>,
    saved_corpus: SavedCorpusIdentity,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    name: String,
    path: String,
    sha256: String,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedCorpusIdentity {
    schema_major: u16,
    deferred: Vec<String>,
    manifest_sha256: String,
    documents: Vec<FileIdentity>,
}

#[derive(Debug, Deserialize)]
struct CorpusManifest {
    schema_major: u16,
    documents: Vec<CorpusDocument>,
    deferred: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CorpusDocument {
    name: String,
    file: String,
}

#[derive(Clone, Copy)]
struct FixtureSpec {
    name: &'static str,
    toolkit: FixtureToolkit,
    display_route: DisplayRoute,
    script: &'static str,
    runner: Runner,
}

#[derive(Clone, Copy)]
enum Runner {
    Python,
    Electron,
}

#[derive(Clone, Copy)]
struct LogicalTargets {
    boundary_a: (f64, f64),
    boundary_b: (f64, f64),
    drag_from: (f64, f64),
    drag_to: (f64, f64),
    centre: (f64, f64),
    entry: (f64, f64),
}

#[derive(Debug)]
struct FixtureSegmentEvidence {
    state: FixtureState,
    event_count: usize,
    response_receipts: usize,
    window_focused: bool,
    workspace_focused: bool,
    fixture_process_removed: bool,
}

const SPECS: [FixtureSpec; 4] = [
    FixtureSpec {
        name: "gtk",
        toolkit: FixtureToolkit::Gtk,
        display_route: DisplayRoute::NativeWayland,
        script: "interaction_gtk.py",
        runner: Runner::Python,
    },
    FixtureSpec {
        name: "qt",
        toolkit: FixtureToolkit::Qt,
        display_route: DisplayRoute::NativeWayland,
        script: "interaction_qt.py",
        runner: Runner::Python,
    },
    FixtureSpec {
        name: "electron",
        toolkit: FixtureToolkit::Electron,
        display_route: DisplayRoute::NativeWayland,
        script: "interaction_electron.js",
        runner: Runner::Electron,
    },
    FixtureSpec {
        name: "xwayland",
        toolkit: FixtureToolkit::Xwayland,
        display_route: DisplayRoute::Xwayland,
        script: "interaction_qt.py",
        runner: Runner::Python,
    },
];

pub fn run(options: Options) -> Result<FinalCampaignSummary, Box<dyn std::error::Error>> {
    let evidence = PrivateRoot::create_retained(&options.evidence_directory)?;
    let identity = campaign_identity(&options)?;
    evidence.write_json("campaign-identity.json", &identity)?;

    let admission = toolkit_preflight::run(toolkit_preflight::Options {
        evidence_directory: options.evidence_directory.join("admission"),
        fixture_directory: options.fixture_directory.clone(),
        python: options.python.clone(),
        electron: options.electron.clone(),
        seed: options.seed,
        probe_only: false,
    })?;

    let mut cells = admission
        .matrix_admission
        .cells
        .iter()
        .map(admission_cell)
        .collect::<Vec<_>>();

    if !options.skip_legacy {
        run_legacy_circuits(&options, &evidence, &mut cells)?;
    }

    let runtime_options = outside_in::Options {
        evidence_directory: options.evidence_directory.clone(),
        waldod: options.waldod.clone(),
        mcp: options.mcp.clone(),
        fixture: options.fixture_directory.join("interaction_gtk.py"),
        python: options.python.clone(),
        seed: options.seed,
    };
    let mut harness = RuntimeHarness::start_prepared(&runtime_options, evidence)?;
    let mut fixture_processes_removed = true;

    for (index, spec) in SPECS.into_iter().enumerate() {
        let admitted = admission
            .toolkits
            .iter()
            .find(|toolkit| toolkit.toolkit == spec.toolkit)
            .is_some_and(|toolkit| {
                toolkit.descriptor_valid
                    && toolkit.final_consumer_ready
                    && toolkit.event_log_valid
                    && toolkit.compositor_window_seen
                    && toolkit.cleanup_complete
            });
        if !admitted {
            cells.push(CampaignCell {
                id: format!("toolkit-{}-journey", spec.name),
                action_families: toolkit_families(spec),
                outcome: CampaignOutcome::Unavailable,
                reason: Some("fixture did not pass exact-candidate admission".to_owned()),
                final_consumers: toolkit_consumers(spec.name),
                evidence: vec![format!("admission/toolkits/{}/result.json", spec.name)],
            });
            continue;
        }

        match run_fixture_segment(&options, &mut harness, spec, index as u64) {
            Ok(segment) => {
                fixture_processes_removed &= segment.fixture_process_removed;
                let passed = segment.state.counters.pointer_moves >= 4
                    && segment.state.counters.pointer_presses >= 3
                    && segment.state.counters.pointer_releases >= 3
                    && segment.state.counters.clicks >= 2
                    && segment.state.counters.drags >= 1
                    && (!requires_scroll(spec)
                        || (segment.state.counters.horizontal_scrolls >= 1
                            && segment.state.counters.vertical_scrolls >= 1))
                    && segment.state.counters.key_presses >= 2
                    && segment.state.counters.key_releases >= 2
                    && segment.state.text_length > 0
                    && segment.event_count >= 16
                    && segment.response_receipts >= 15
                    && segment.window_focused
                    && segment.workspace_focused;
                let mut evidence_paths = vec![
                    format!("toolkits/{}/request.json", spec.name),
                    format!("toolkits/{}/final-state.json", spec.name),
                    format!("toolkits/{}/events.json", spec.name),
                    format!("toolkits/{}/act-response.json", spec.name),
                    format!("toolkits/{}/window-focus-response.json", spec.name),
                    format!("toolkits/{}/workspace-focus-response.json", spec.name),
                ];
                if spec.toolkit == FixtureToolkit::Electron {
                    evidence_paths.extend([
                        format!("toolkits/{}/calibration-request.json", spec.name),
                        format!("toolkits/{}/calibration-response.json", spec.name),
                    ]);
                }
                cells.push(CampaignCell {
                    id: format!("toolkit-{}-journey", spec.name),
                    action_families: toolkit_families(spec),
                    outcome: if passed {
                        CampaignOutcome::Passed
                    } else {
                        CampaignOutcome::Failed
                    },
                    reason: (!passed).then(|| {
                        "one or more action families lacked distinctive application/compositor evidence"
                            .to_owned()
                    }),
                    final_consumers: toolkit_consumers(spec.name),
                    evidence: evidence_paths,
                });
            }
            Err(error) => {
                let failure = format!("failures/toolkit-{}.json", spec.name);
                harness
                    .evidence()
                    .write_json(&failure, &json!({"error": error.to_string()}))?;
                cells.push(CampaignCell {
                    id: format!("toolkit-{}-journey", spec.name),
                    action_families: toolkit_families(spec),
                    outcome: CampaignOutcome::Failed,
                    reason: Some(error.to_string()),
                    final_consumers: toolkit_consumers(spec.name),
                    evidence: vec![failure],
                });
            }
        }
    }

    let coherent_act = match run_coherent_act(&options, &mut harness) {
        Ok(evidence) => {
            fixture_processes_removed &= evidence.fixture_process_removed;
            let passed = evidence.pointer_move_distinctive
                && evidence.drag_distinctive
                && evidence.click_distinctive
                && evidence.text_distinctive
                && evidence.wait_process_exited
                && evidence.capture_returned
                && evidence.screenshot_changed
                && evidence.ordered_receipts;
            cells.push(CampaignCell {
                id: "coherent-act".to_owned(),
                action_families: vec![
                    "pointer".to_owned(),
                    "drag".to_owned(),
                    "click".to_owned(),
                    "virtual-keyboard text".to_owned(),
                    "process-exited wait".to_owned(),
                    "capture".to_owned(),
                ],
                outcome: if passed {
                    CampaignOutcome::Passed
                } else {
                    CampaignOutcome::Failed
                },
                reason: (!passed).then(|| {
                    "the coherent Act lacked at least one distinctive final-consumer discriminator"
                        .to_owned()
                }),
                final_consumers: vec![
                    "GTK event journal and digest-only state".to_owned(),
                    "process exit state".to_owned(),
                    "returned screenshot digest".to_owned(),
                ],
                evidence: vec![
                    evidence.saved_document.clone(),
                    "coherent-act/final-state.json".to_owned(),
                    "coherent-act/events.json".to_owned(),
                    "coherent-act/act-response.json".to_owned(),
                    "coherent-act/result.json".to_owned(),
                ],
            });
            Some(evidence)
        }
        Err(error) => {
            let failure = "failures/coherent-act.json";
            harness
                .evidence()
                .write_json(failure, &json!({"error": error.to_string()}))?;
            cells.push(CampaignCell {
                id: "coherent-act".to_owned(),
                action_families: vec![
                    "pointer".to_owned(),
                    "drag".to_owned(),
                    "click".to_owned(),
                    "virtual-keyboard text".to_owned(),
                    "process-exited wait".to_owned(),
                    "capture".to_owned(),
                ],
                outcome: CampaignOutcome::Failed,
                reason: Some(error.to_string()),
                final_consumers: vec![
                    "GTK event journal and digest-only state".to_owned(),
                    "process exit state".to_owned(),
                    "returned screenshot digest".to_owned(),
                ],
                evidence: vec![failure.to_owned()],
            });
            None
        }
    };

    let (runtime_identity, controller_runtime_removed) = harness.shutdown()?;
    write_json(
        options.evidence_directory.join("runtime-identity.json"),
        &runtime_identity,
    )?;

    let coverage = FinalCampaignRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Final,
        binding_shapes_settled: true,
        corpus_schema_major: identity.saved_corpus.schema_major,
        generated_at: generated_at()?,
        cells,
    };
    coverage.validate()?;
    verify_evidence_pointers(&options.evidence_directory, &coverage.cells)?;
    let coverage_totals = totals(&coverage);
    write_json(options.evidence_directory.join("coverage.json"), &coverage)?;

    let privacy_marker = format!("waldo-final-private-{:016x}", options.seed);
    let privacy_scan = scan_private_root(
        &options.evidence_directory,
        &[("final-private-marker", privacy_marker.as_bytes())],
    )?;
    write_json(
        options.evidence_directory.join("privacy-scan.json"),
        &privacy_scan,
    )?;

    let failed = coverage_totals.failed > 0
        || options.skip_legacy
        || !identity.source_tree_clean
        || !controller_runtime_removed
        || !fixture_processes_removed
        || !privacy_scan.leaks.is_empty();
    let summary = FinalCampaignSummary {
        evidence_maturity: EvidenceMaturity::Final,
        binding_shapes_settled: true,
        corpus_schema_major: identity.saved_corpus.schema_major,
        source_commit: identity.source_commit,
        source_tree_clean: identity.source_tree_clean,
        coverage,
        coverage_totals,
        coherent_act,
        controller_runtime_removed,
        fixture_processes_removed,
        sensitive_payloads_retained: !privacy_scan.leaks.is_empty(),
        privacy_scan,
        failed,
    };
    write_json(options.evidence_directory.join("summary.json"), &summary)?;
    if failed {
        return Err("final desktop conformance campaign failed; inspect retained evidence".into());
    }
    Ok(summary)
}

fn run_legacy_circuits(
    options: &Options,
    evidence: &PrivateRoot,
    cells: &mut Vec<CampaignCell>,
) -> Result<(), Box<dyn std::error::Error>> {
    let circuits = [
        (
            "coordinate-completion",
            "coordinate_grid.py",
            options.seed.wrapping_add(0x100),
            false,
        ),
        (
            "accessibility",
            "accessibility.py",
            options.seed.wrapping_add(0x200),
            false,
        ),
        (
            "clipboard",
            "clipboard_text.py",
            options.seed.wrapping_add(0x300),
            true,
        ),
    ];
    for (name, fixture, seed, clipboard) in circuits {
        let relative = format!("legacy/{name}");
        let run_options = outside_in::Options {
            evidence_directory: options.evidence_directory.join(&relative),
            waldod: options.waldod.clone(),
            mcp: options.mcp.clone(),
            fixture: options.fixture_directory.join(fixture),
            python: options.python.clone(),
            seed,
        };
        let result = if clipboard {
            clipboard_outside_in::run(run_options).map(|_| ())
        } else if name == "accessibility" {
            outside_in::run_accessibility(run_options).map(|_| ())
        } else {
            outside_in::run(run_options).map(|_| ())
        };
        match result {
            Ok(()) => cells.push(CampaignCell {
                id: format!("legacy-{name}"),
                action_families: vec![name.to_owned()],
                outcome: CampaignOutcome::Passed,
                reason: None,
                final_consumers: vec!["independent outside-in application oracle".to_owned()],
                evidence: vec![format!("{relative}/summary.json")],
            }),
            Err(error) => {
                let failure = format!("failures/legacy-{name}.json");
                evidence.write_json(&failure, &json!({"error": error.to_string()}))?;
                cells.push(CampaignCell {
                    id: format!("legacy-{name}"),
                    action_families: vec![name.to_owned()],
                    outcome: CampaignOutcome::Failed,
                    reason: Some(error.to_string()),
                    final_consumers: vec!["independent outside-in application oracle".to_owned()],
                    evidence: vec![failure],
                });
            }
        }
    }
    Ok(())
}

fn run_fixture_segment(
    options: &Options,
    harness: &mut RuntimeHarness,
    spec: FixtureSpec,
    offset: u64,
) -> Result<FixtureSegmentEvidence, Box<dyn std::error::Error>> {
    let seed = options.seed.wrapping_add(0x1000 + offset);
    let paths = fixture_paths(harness.scratch(), "toolkit", spec.name)?;
    let mut fixture = launch_fixture(options, spec, seed, &paths)?;
    let fixture_pid = fixture.0.id();
    let initial = wait_for_fixture_state(&paths.state, |state| state.ready)?;
    focus_niri_window(&initial.descriptor.title)?;
    let directory = harness
        .evidence()
        .create_directory(format!("toolkits/{}", spec.name))?;
    let (window_id, workspace_id, output_id) = fixture_window(&initial.descriptor.title)?;
    make_window_floating(window_id)?;
    let (observation, target_state, calibration_receipts) =
        if spec.toolkit == FixtureToolkit::Electron {
            let observed = outside_in::observe_without_accessibility(
                &mut harness.mcp,
                &output_id,
                &initial.descriptor.title,
                &directory,
                "initial",
            )?;
            let calibration_request = json!({
                "observation_id": observed.id,
                "actions": [{
                    "type": "pointer_move_absolute",
                    "position": point(observed.logical_centre())
                }],
                "verification": []
            });
            harness.evidence().write_json(
                format!("toolkits/{}/calibration-request.json", spec.name),
                &calibration_request,
            )?;
            let calibration_response = harness
                .mcp
                .call_with_timeout(
                    "tools/call",
                    json!({"name": "act", "arguments": calibration_request}),
                    Duration::from_secs(30),
                )?
                .ok_or("Electron input-origin calibration Act exceeded 30 seconds")?;
            let calibrated_observation =
                parse_observation(&calibration_response, &directory, "calibration-result")?;
            write_redacted_response(
                directory.join("calibration-response.json"),
                &calibration_response,
                &calibrated_observation,
            )?;
            let calibrated_state = wait_for_fixture_state(&paths.state, |state| {
                state.target_geometry.is_some_and(|geometry| {
                    geometry.screen_x.is_some() && geometry.screen_y.is_some()
                })
            })?;
            let receipt_count = executed_receipts(&calibration_response);
            (calibrated_observation, calibrated_state, receipt_count)
        } else {
            let observed = observe_window_without_accessibility(
                &mut harness.mcp,
                &window_id.to_string(),
                &initial.descriptor.title,
                &directory,
                "initial",
            )?;
            (observed, initial.clone(), 0)
        };
    let targets = targets(&observation, seed, spec, &target_state)?;
    let marker = format!("{}-v2-{:04x}", spec.name, seed & 0xffff);
    let request = json!({
        "observation_id": observation.id,
        "actions": [
            {"type": "pointer_move_absolute", "position": point(targets.boundary_a)},
            {"type": "pointer_move_absolute", "position": point(targets.boundary_b)},
            {"type": "pointer_move_relative", "delta": {"x": -4.0, "y": -3.0}},
            {"type": "pointer_button", "button": "primary", "state": "pressed"},
            {"type": "pointer_button", "button": "primary", "state": "released"},
            {"type": "pointer_click", "position": point(targets.centre), "button": "primary", "count": 1},
            {"type": "pointer_drag", "from": point(targets.drag_from), "to": point(targets.drag_to), "button": "primary", "duration_ms": 160},
            {"type": "pointer_path", "points": [point(targets.drag_to), point(targets.centre)], "duration_ms": 120},
            {"type": "scroll", "delta": {"x": 5.0, "y": 7.0}},
            {"type": "pointer_click", "position": point(targets.entry), "button": "primary", "count": 1},
            {"type": "key", "key": "a", "state": "pressed"},
            {"type": "key", "key": "a", "state": "released"},
            {"type": "key_chord", "keys": ["ctrl", "a"]},
            {"type": "text", "text": marker, "route": "virtual_keyboard"}
        ],
        "verification": []
    });
    harness
        .evidence()
        .write_json(format!("toolkits/{}/request.json", spec.name), &request)?;
    let response = harness
        .mcp
        .call_with_timeout(
            "tools/call",
            json!({"name": "act", "arguments": request}),
            Duration::from_secs(45),
        )?
        .ok_or("toolkit Act exceeded the 45-second campaign limit")?;
    let final_observation = parse_observation(&response, &directory, "act-result")?;
    write_redacted_response(
        directory.join("act-response.json"),
        &response,
        &final_observation,
    )?;
    let (window_response, workspace_response) = run_compositor_actions(
        &mut harness.mcp,
        &directory,
        &final_observation,
        window_id,
        workspace_id,
    )?;
    let digest = sha256_bytes(marker.as_bytes())?;
    let state = wait_for_fixture_state(&paths.state, |state| {
        state.text_sha256.as_deref() == Some(digest.as_str())
            && state.counters.drags >= 1
            && (!requires_scroll(spec)
                || (state.counters.horizontal_scrolls >= 1 && state.counters.vertical_scrolls >= 1))
    })
    .or_else(|error| {
        retain_failed_fixture_oracle(harness.evidence(), spec.name, &paths, &error.to_string())?;
        Err(error)
    })?;
    let events = complete_fixture_events(&fs::read_to_string(&paths.events)?)?;
    harness
        .evidence()
        .write_json(format!("toolkits/{}/final-state.json", spec.name), &state)?;
    harness
        .evidence()
        .write_json(format!("toolkits/{}/events.json", spec.name), &events)?;
    let (window_focused, workspace_focused) = final_compositor_state(window_id, workspace_id)?;
    fixture.terminate_checked()?;
    let fixture_process_removed = !Path::new("/proc").join(fixture_pid.to_string()).exists();
    Ok(FixtureSegmentEvidence {
        state,
        event_count: events.len(),
        response_receipts: executed_receipts(&response)
            + executed_receipts(&window_response)
            + executed_receipts(&workspace_response)
            + calibration_receipts,
        window_focused,
        workspace_focused,
        fixture_process_removed,
    })
}

fn run_compositor_actions(
    mcp: &mut outside_in::McpClient,
    directory: &Path,
    observation: &ToolObservation,
    window_id: u64,
    workspace_id: u64,
) -> Result<(Value, Value), Box<dyn std::error::Error>> {
    let window_response = mcp
        .call_with_timeout(
            "tools/call",
            json!({
                "name": "act",
                "arguments": {
                    "observation_id": observation.id,
                    "actions": [{
                        "type": "window",
                        "window_id": window_id.to_string(),
                        "operation": {"type": "focus"}
                    }],
                    "verification": []
                }
            }),
            Duration::from_secs(30),
        )?
        .ok_or("window-focus Act exceeded the 30-second campaign limit")?;
    let window_observation = parse_observation(&window_response, directory, "window-focus")?;
    write_redacted_response(
        directory.join("window-focus-response.json"),
        &window_response,
        &window_observation,
    )?;
    let workspace_response = mcp
        .call_with_timeout(
            "tools/call",
            json!({
                "name": "act",
                "arguments": {
                    "observation_id": window_observation.id,
                    "actions": [{
                        "type": "workspace",
                        "workspace_id": workspace_id,
                        "operation": {"type": "focus"}
                    }],
                    "verification": []
                }
            }),
            Duration::from_secs(30),
        )?
        .ok_or("workspace-focus Act exceeded the 30-second campaign limit")?;
    let workspace_observation =
        parse_observation(&workspace_response, directory, "workspace-focus")?;
    write_redacted_response(
        directory.join("workspace-focus-response.json"),
        &workspace_response,
        &workspace_observation,
    )?;
    Ok((window_response, workspace_response))
}

fn run_coherent_act(
    options: &Options,
    harness: &mut RuntimeHarness,
) -> Result<CoherentActEvidence, Box<dyn std::error::Error>> {
    let spec = SPECS[0];
    let seed = options.seed.wrapping_add(0x2000);
    let paths = fixture_paths(harness.scratch(), "coherent", spec.name)?;
    let mut fixture = launch_fixture(options, spec, seed, &paths)?;
    let fixture_pid = fixture.0.id();
    let initial = wait_for_fixture_state(&paths.state, |state| state.ready)?;
    focus_niri_window(&initial.descriptor.title)?;
    let directory = harness.evidence().create_directory("coherent-act")?;
    let (window_id, _, _) = fixture_window(&initial.descriptor.title)?;
    make_window_floating(window_id)?;
    let observation = observe_window_without_accessibility(
        &mut harness.mcp,
        &window_id.to_string(),
        &initial.descriptor.title,
        &directory,
        "initial",
    )?;
    let targets = targets(&observation, seed, spec, &initial)?;

    let mut wait_process = wait_command().spawn()?;
    let wait_pid = wait_process.id();
    let wait_reaper = thread::spawn(move || wait_process.wait());
    let template_path = options.corpus_directory.join("coherent-act.json");
    let mut request: Value = serde_json::from_slice(&fs::read(&template_path)?)?;
    request["observation_id"] = observation.id.into();
    request["actions"][0]["position"] = point(targets.boundary_a);
    request["actions"][1]["from"] = point(targets.drag_from);
    request["actions"][1]["to"] = point(targets.drag_to);
    request["actions"][2]["position"] = point(targets.entry);
    request["actions"][3]["text"] = "coherent-v2".into();
    request["actions"][4]["predicate"]["pid"] = wait_pid.into();
    request["actions"][4]["timeout_ms"] = 5000.into();
    request["actions"][5]["source"] = observation.source.clone();
    let saved_document = "saved/v2/coherent-act-exact.json";
    let saved_path = harness.evidence().write_json(saved_document, &request)?;
    let saved_document_sha256 = sha256_bytes(&fs::read(&saved_path)?)?;

    let response = harness
        .mcp
        .call_with_timeout(
            "tools/call",
            json!({"name": "act", "arguments": request}),
            Duration::from_secs(45),
        )?
        .ok_or("coherent Act exceeded the 45-second campaign limit")?;
    let final_observation = parse_observation(&response, &directory, "act-result")?;
    write_redacted_response(
        directory.join("act-response.json"),
        &response,
        &final_observation,
    )?;
    let wait_process_exited = wait_reaper
        .join()
        .map_err(|_| "wait-process reaper thread panicked")??
        .success();
    let text_digest = sha256_bytes(b"coherent-v2")?;
    let state = wait_for_fixture_state(&paths.state, |state| {
        state.text_sha256.as_deref() == Some(text_digest.as_str())
            && state.counters.drags >= 1
            && state.counters.clicks >= 2
    })
    .or_else(|error| {
        retain_failed_fixture_oracle(
            harness.evidence(),
            "coherent-act",
            &paths,
            &error.to_string(),
        )?;
        Err(error)
    })?;
    let events = complete_fixture_events(&fs::read_to_string(&paths.events)?)?;
    harness
        .evidence()
        .write_json("coherent-act/final-state.json", &state)?;
    harness
        .evidence()
        .write_json("coherent-act/events.json", &events)?;
    fixture.terminate_checked()?;
    let fixture_process_removed = !Path::new("/proc").join(fixture_pid.to_string()).exists();

    let evidence = CoherentActEvidence {
        saved_document: saved_document.to_owned(),
        saved_document_sha256,
        pointer_move_distinctive: state.counters.pointer_moves >= 2,
        drag_distinctive: state.counters.drags >= 1,
        click_distinctive: state.counters.clicks >= 2,
        text_distinctive: state.text_sha256.as_deref() == Some(text_digest.as_str()),
        wait_process_exited,
        capture_returned: !final_observation.image_digest().is_empty(),
        screenshot_changed: observation.image_digest() != final_observation.image_digest(),
        ordered_receipts: executed_receipts(&response) == 6,
        application_event_count: events.len(),
        final_image_sha256: final_observation.image_digest().to_owned(),
        fixture_process_removed,
    };
    harness
        .evidence()
        .write_json("coherent-act/result.json", &evidence)?;
    Ok(evidence)
}

struct FixturePaths {
    state: PathBuf,
    events: PathBuf,
    control: PathBuf,
}

fn fixture_paths(
    scratch: &PrivateRoot,
    group: &str,
    name: &str,
) -> Result<FixturePaths, Box<dyn std::error::Error>> {
    let directory = scratch.create_directory(format!("fixtures/{group}/{name}"))?;
    let control = directory.join("control.json");
    write_json(control.clone(), &json!({"sequence": 0}))?;
    Ok(FixturePaths {
        state: directory.join("state.json"),
        events: directory.join("events.jsonl"),
        control,
    })
}

fn launch_fixture(
    options: &Options,
    spec: FixtureSpec,
    seed: u64,
    paths: &FixturePaths,
) -> Result<ChildGuard, Box<dyn std::error::Error>> {
    let script = options.fixture_directory.join(spec.script);
    let mut command = match spec.runner {
        Runner::Python => {
            let mut command = Command::new(&options.python);
            command.env("PYTHONDONTWRITEBYTECODE", "1");
            command
        }
        Runner::Electron => Command::new(&options.electron),
    };
    command
        .arg(script)
        .args(["--state", path_text(&paths.state)?])
        .args(["--events", path_text(&paths.events)?])
        .args(["--control", path_text(&paths.control)?])
        .args(["--seed", &seed.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if spec.toolkit == FixtureToolkit::Gtk {
        command.env("GDK_BACKEND", "wayland");
    }
    if spec.display_route == DisplayRoute::Xwayland {
        command.args(["--display-route", "xwayland"]);
    }
    Ok(ChildGuard(command.spawn()?))
}

fn wait_for_fixture_state(
    path: &Path,
    predicate: impl Fn(&FixtureState) -> bool,
) -> Result<FixtureState, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;
    loop {
        match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<FixtureState>(&bytes) {
                Ok(state) if predicate(&state) => return Ok(state),
                Ok(_) => {}
                Err(error) => last_error = Some(error.to_string()),
            },
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                last_error = Some(error.to_string());
            }
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out waiting for fixture state {}: {}",
                path.display(),
                last_error.unwrap_or_else(|| "predicate remained false".to_owned())
            )
            .into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn targets(
    observation: &ToolObservation,
    seed: u64,
    spec: FixtureSpec,
    initial: &FixtureState,
) -> Result<LogicalTargets, Box<dyn std::error::Error>> {
    if spec.toolkit == FixtureToolkit::Electron {
        let geometry = initial
            .target_geometry
            .ok_or("Electron fixture did not publish target geometry")?;
        let screen_x = geometry
            .screen_x
            .ok_or("Electron fixture input origin x was not calibrated")?;
        let screen_y = geometry
            .screen_y
            .ok_or("Electron fixture input origin y was not calibrated")?;
        let logical = |x, y| (screen_x + x, screen_y + y);
        let surface = geometry.surface;
        let entry = geometry.entry;
        return Ok(LogicalTargets {
            boundary_a: logical(surface.x + 5.0, surface.y + 5.0),
            boundary_b: logical(
                surface.x + surface.width - 5.0,
                surface.y + surface.height - 5.0,
            ),
            drag_from: logical(
                surface.x + surface.width / 3.0,
                surface.y + surface.height / 2.0,
            ),
            drag_to: logical(
                surface.x + surface.width * 2.0 / 3.0,
                surface.y + surface.height * 5.0 / 8.0,
            ),
            centre: logical(
                surface.x + surface.width / 2.0,
                surface.y + surface.height / 2.0,
            ),
            entry: logical(entry.x + entry.width / 2.0, entry.y + entry.height / 2.0),
        });
    }
    let expected = [
        (32 + seed * 17 % 192) as u8,
        (32 + seed * 29 % 192) as u8,
        (32 + seed * 43 % 192) as u8,
    ];
    let (min_x, min_y, max_x, max_y) = (0..=96)
        .step_by(8)
        .filter_map(|tolerance| observation.largest_colour_region(expected, tolerance))
        .find(|(min_x, min_y, max_x, max_y)| {
            max_x.saturating_sub(*min_x) >= 160 && max_y.saturating_sub(*min_y) >= 80
        })
        .ok_or_else(|| {
            let (closest, distance, x, y) = observation.closest_colour(expected);
            format!(
                "fixture surface colour {expected:?} had no safe connected region through tolerance 96; closest delivered RGBA {closest:?} had max-channel distance {distance} at ({x},{y})"
            )
        })?;
    let width = max_x - min_x;
    let height = max_y - min_y;
    let entry_y = max_y
        .saturating_add((height / 10).clamp(10, 38))
        .min(observation.model_dimensions().1.saturating_sub(2));
    let logical = |x, y| observation.model_to_logical_point(x, y);
    Ok(LogicalTargets {
        boundary_a: logical(min_x + 5, min_y + 5),
        boundary_b: logical(max_x - 5, max_y - 5),
        drag_from: logical(min_x + width / 3, min_y + height / 2),
        drag_to: logical(min_x + width * 2 / 3, min_y + height / 2 + height / 8),
        centre: logical(min_x + width / 2, min_y + height / 2),
        entry: logical(min_x + width / 2, entry_y),
    })
}

fn retain_failed_fixture_oracle(
    evidence: &PrivateRoot,
    label: &str,
    paths: &FixturePaths,
    error: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = fs::read(&paths.state)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FixtureState>(&bytes).ok());
    let events = fs::read_to_string(&paths.events)
        .ok()
        .and_then(|body| complete_fixture_events(&body).ok());
    evidence.write_json(
        format!("failures/{label}-oracle.json"),
        &json!({"error": error, "state": state, "events": events}),
    )?;
    Ok(())
}

fn point(value: (f64, f64)) -> Value {
    json!({"x": value.0, "y": value.1})
}

fn fixture_window(title: &str) -> Result<(u64, u64, String), Box<dyn std::error::Error>> {
    let mut socket = Socket::connect()?;
    let Response::Windows(windows) = socket.send(Request::Windows)?? else {
        return Err("Niri returned an unexpected response to Windows".into());
    };
    let matches = windows
        .into_iter()
        .filter(|window| window.title.as_deref() == Some(title))
        .collect::<Vec<_>>();
    let [window] = matches.as_slice() else {
        return Err(format!("expected one Niri fixture window titled {title:?}").into());
    };
    let workspace_id = window
        .workspace_id
        .ok_or("fixture window has no Niri workspace id")?;
    let mut socket = Socket::connect()?;
    let Response::Workspaces(workspaces) = socket.send(Request::Workspaces)?? else {
        return Err("Niri returned an unexpected response to Workspaces".into());
    };
    let output_id = workspaces
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .and_then(|workspace| workspace.output)
        .ok_or("fixture workspace has no Niri output")?;
    Ok((window.id, workspace_id, output_id))
}

fn final_compositor_state(
    window_id: u64,
    workspace_id: u64,
) -> Result<(bool, bool), Box<dyn std::error::Error>> {
    let mut socket = Socket::connect()?;
    let Response::Windows(windows) = socket.send(Request::Windows)?? else {
        return Err("Niri returned an unexpected response to Windows".into());
    };
    let window_focused = windows
        .iter()
        .any(|window| window.id == window_id && window.is_focused);
    let mut socket = Socket::connect()?;
    let Response::Workspaces(workspaces) = socket.send(Request::Workspaces)?? else {
        return Err("Niri returned an unexpected response to Workspaces".into());
    };
    let workspace_focused = workspaces
        .iter()
        .any(|workspace| workspace.id == workspace_id && workspace.is_focused);
    Ok((window_focused, workspace_focused))
}

fn make_window_floating(window_id: u64) -> Result<(), Box<dyn std::error::Error>> {
    let mut socket = Socket::connect()?;
    let reply = socket.send(Request::Action(NiriAction::MoveWindowToFloating {
        id: Some(window_id),
    }))??;
    if !matches!(reply, Response::Handled) {
        return Err("Niri did not acknowledge moving the fixture window to floating".into());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut socket = Socket::connect()?;
        let Response::Windows(windows) = socket.send(Request::Windows)?? else {
            return Err("Niri returned an unexpected response to Windows".into());
        };
        if windows
            .iter()
            .any(|window| window.id == window_id && window.is_floating)
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("fixture window did not become floating before capture".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn executed_receipts(response: &Value) -> usize {
    response["result"]["structuredContent"]["result"]["receipts"]
        .as_array()
        .map(|receipts| {
            receipts
                .iter()
                .take_while(|receipt| receipt["status"] == "executed")
                .count()
        })
        .unwrap_or(0)
}

fn campaign_identity(options: &Options) -> Result<CampaignIdentity, Box<dyn std::error::Error>> {
    let manifest_path = options.corpus_directory.join("manifest.json");
    let manifest: CorpusManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if manifest.schema_major != 2 || manifest.deferred != ["mcp-handover"] {
        return Err("saved corpus is not the settled v2 corpus with MCP handover deferred".into());
    }
    let mut documents = Vec::new();
    for document in manifest.documents {
        let path = options.corpus_directory.join(&document.file);
        documents.push(file_identity(&document.name, &path)?);
    }
    let mut files = vec![
        file_identity("waldod", &options.waldod)?,
        file_identity("waldo-mcp", &options.mcp)?,
        file_identity("waldo-cli", &options.cli)?,
        file_identity("plugin-manifest", &options.plugin_manifest)?,
        file_identity("waldo-verification", &std::env::current_exe()?)?,
    ];
    for spec in SPECS {
        files.push(file_identity(
            &format!("{}-fixture", spec.name),
            &options.fixture_directory.join(spec.script),
        )?);
    }
    files.push(file_identity(
        "interaction-common",
        &options.fixture_directory.join("interaction_common.py"),
    )?);
    files.push(file_identity(
        "electron-html",
        &options.fixture_directory.join("interaction_electron.html"),
    )?);
    Ok(CampaignIdentity {
        source_commit: git_output(&["rev-parse", "HEAD"])?,
        source_tree_clean: git_output(&["status", "--porcelain"])?.is_empty(),
        seed: options.seed,
        files,
        saved_corpus: SavedCorpusIdentity {
            schema_major: manifest.schema_major,
            deferred: manifest.deferred,
            manifest_sha256: sha256_bytes(&fs::read(&manifest_path)?)?,
            documents,
        },
    })
}

fn file_identity(name: &str, path: &Path) -> Result<FileIdentity, Box<dyn std::error::Error>> {
    Ok(FileIdentity {
        name: name.to_owned(),
        path: path.display().to_string(),
        sha256: sha256_bytes(&fs::read(path)?)?,
    })
}

fn git_output(arguments: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git").args(arguments).output()?;
    if !output.status.success() {
        return Err(format!("git {} failed", arguments.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn admission_cell(cell: &waldo_verification::MatrixCellAdmission) -> CampaignCell {
    CampaignCell {
        id: format!("environment-{}-{}", cell.dimension, cell.value),
        action_families: vec!["environment admission".to_owned()],
        outcome: if cell.outcome == AdmissionOutcome::Available {
            CampaignOutcome::Passed
        } else {
            CampaignOutcome::Unavailable
        },
        reason: cell.reason.clone(),
        final_consumers: vec!["runtime-discovered environment state".to_owned()],
        evidence: cell
            .evidence
            .iter()
            .map(|path| format!("admission/{path}"))
            .collect(),
    }
}

fn toolkit_families(spec: FixtureSpec) -> Vec<String> {
    let mut families = vec![
        "pointer absolute/relative/button/click/drag/path",
        "key press/release and chord",
        "virtual-keyboard text",
        "window focus",
        "workspace focus",
        "boundary and model-image mapping",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    if requires_scroll(spec) {
        families.push("horizontal and vertical scroll".to_owned());
    }
    families
}

fn requires_scroll(spec: FixtureSpec) -> bool {
    matches!(spec.toolkit, FixtureToolkit::Gtk | FixtureToolkit::Qt)
}

fn toolkit_consumers(name: &str) -> Vec<String> {
    vec![
        format!("{name} application event journal"),
        format!("{name} digest-only application state"),
        "Niri window/workspace state".to_owned(),
        "returned screenshot coordinates".to_owned(),
    ]
}

fn verify_evidence_pointers(
    root: &Path,
    cells: &[CampaignCell],
) -> Result<(), Box<dyn std::error::Error>> {
    for pointer in cells.iter().flat_map(|cell| &cell.evidence) {
        let relative = Path::new(pointer);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "campaign evidence pointer is not a safe relative path: {pointer}"
            )
            .into());
        }
        let path = root.join(relative);
        let metadata = path.symlink_metadata().map_err(|error| {
            format!(
                "campaign evidence pointer {} is missing: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "campaign evidence pointer {} is not a regular file",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

fn totals(record: &FinalCampaignRecord) -> CoverageTotals {
    let mut totals = BTreeMap::new();
    for cell in &record.cells {
        *totals.entry(format!("{:?}", cell.outcome)).or_insert(0) += 1;
    }
    CoverageTotals {
        passed: *totals.get("Passed").unwrap_or(&0),
        failed: *totals.get("Failed").unwrap_or(&0),
        inconclusive: *totals.get("Inconclusive").unwrap_or(&0),
        unavailable: *totals.get("Unavailable").unwrap_or(&0),
    }
}

fn generated_at() -> Result<String, Box<dyn std::error::Error>> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(format!("unix:{seconds}"))
}

fn path_text(path: &Path) -> Result<&str, Box<dyn std::error::Error>> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path: {}", path.display()).into())
}

fn wait_command() -> Command {
    let mut command = Command::new("sleep");
    command.arg("2").stdout(Stdio::null()).stderr(Stdio::null());
    command
}

#[cfg(test)]
#[path = "tests/sleep_defaults.rs"]
mod sleep_defaults;
