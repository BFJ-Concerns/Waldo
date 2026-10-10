use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use niri_ipc::{Request, Response, socket::Socket};
use serde::Serialize;
use serde_json::json;
use waldo_verification::{
    AdmissionOutcome, CONFORMANCE_SCHEMA_VERSION, DisplayRoute, EvidenceMaturity,
    FixtureDescriptor, FixtureState, FixtureToolkit, MatrixAdmissionRecord, MatrixCellAdmission,
    PrimitiveOutcome, PrivacyScan, PrivateRoot, ScenarioPrimitiveReadiness,
    ScenarioReadinessRecord, TopologySnapshot, TopologyTransform, capture_niri_topology,
    complete_fixture_events,
};

const FIXTURE_RELEASE: &str = "interaction-fixtures-v1";
const CAMPAIGN_BLOCKER: &str =
    "fixture admission is complete, but the final action campaign has not run";

#[derive(Clone, Debug)]
pub struct Options {
    pub evidence_directory: PathBuf,
    pub fixture_directory: PathBuf,
    pub python: PathBuf,
    pub electron: PathBuf,
    pub seed: u64,
    pub probe_only: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightSummary {
    pub evidence_maturity: EvidenceMaturity,
    pub binding_shapes_settled: bool,
    pub fixture_release: String,
    pub matrix_admission: MatrixAdmissionRecord,
    pub scenario_readiness: ScenarioReadinessRecord,
    pub toolkits: Vec<ToolkitPreflight>,
    pub scratch_removed: bool,
    pub privacy_scan: PrivacyScan,
    pub finalisation_blockers: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolkitPreflight {
    pub toolkit: FixtureToolkit,
    pub display_route: DisplayRoute,
    pub descriptor_valid: bool,
    pub runtime_backend: Option<String>,
    pub graphical_launch_attempted: bool,
    pub final_consumer_ready: bool,
    pub event_log_valid: bool,
    pub event_count: u64,
    pub event_log_sha256: Option<String>,
    pub compositor_window_seen: bool,
    pub x11_root_window_seen: Option<bool>,
    pub cleanup_complete: bool,
    pub limitation: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct PreflightIdentity {
    commit: String,
    working_tree_dirty: bool,
    verification_diff_sha256: String,
    files: Vec<FileIdentity>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    name: String,
    path: String,
    sha256: String,
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

pub fn run(options: Options) -> Result<PreflightSummary, Box<dyn std::error::Error>> {
    let evidence = PrivateRoot::create_retained(&options.evidence_directory)?;
    let scratch = PrivateRoot::create_scratch("waldo-toolkit-preflight")?;
    let scratch_path = scratch.path().to_path_buf();
    let environment = environment_evidence();
    evidence.write_json("environment.json", &environment)?;
    let identity = identity_evidence(&options)?;
    evidence.write_json("identity.json", &identity)?;

    let topology = capture_niri_topology().ok();
    evidence.write_json("topology.json", &topology)?;
    let keyboard_layouts = capture_keyboard_layouts().ok();
    evidence.write_json("keyboard-layouts.json", &keyboard_layouts)?;

    let specs = [
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

    let mut toolkits = Vec::new();
    for spec in specs {
        toolkits.push(preflight_fixture(&options, &evidence, &scratch, spec)?);
    }

    let cells = matrix_cells(&toolkits, topology.as_ref(), keyboard_layouts.as_deref());
    let matrix_admission = MatrixAdmissionRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Final,
        binding_shapes_settled: true,
        fixture_release: FIXTURE_RELEASE.to_owned(),
        generated_at: generated_at()?,
        finalisation_blockers: Vec::new(),
        cells,
    };
    matrix_admission.validate()?;
    evidence.write_json("matrix-admission.json", &matrix_admission)?;
    let scenario_readiness = scenario_readiness(&toolkits, topology.is_some())?;
    evidence.write_json("scenario-readiness.json", &scenario_readiness)?;

    let privacy_marker = format!("waldo-private-marker-{:016x}", options.seed);
    evidence.write_json(
        "privacy-challenge.json",
        &json!({
            "label": "generated-marker",
            "length": privacy_marker.len(),
            "sha256": sha256_bytes(privacy_marker.as_bytes())?,
        }),
    )?;
    let privacy_scan =
        evidence.scan_for_payloads(&[("generated-marker", privacy_marker.as_bytes())])?;
    evidence.write_json("privacy-scan.json", &privacy_scan)?;

    scratch.remove()?;
    let scratch_removed = !scratch_path.exists();
    let summary = PreflightSummary {
        evidence_maturity: EvidenceMaturity::Provisional,
        binding_shapes_settled: true,
        fixture_release: FIXTURE_RELEASE.to_owned(),
        matrix_admission,
        scenario_readiness,
        toolkits,
        scratch_removed,
        privacy_scan,
        finalisation_blockers: vec![CAMPAIGN_BLOCKER.to_owned()],
    };
    evidence.write_json("summary.json", &summary)?;
    Ok(summary)
}

fn preflight_fixture(
    options: &Options,
    evidence: &PrivateRoot,
    scratch: &PrivateRoot,
    spec: FixtureSpec,
) -> Result<ToolkitPreflight, Box<dyn std::error::Error>> {
    let script = options.fixture_directory.join(spec.script);
    let evidence_prefix = format!("toolkits/{}", spec.name);
    let scratch_prefix = format!("toolkits/{}", spec.name);
    scratch.create_directory(&scratch_prefix)?;
    let state = scratch.path().join(&scratch_prefix).join("state.json");
    let events = scratch.path().join(&scratch_prefix).join("events.jsonl");
    let control_relative = format!("{scratch_prefix}/control.json");
    let control = scratch.write_json(&control_relative, &json!({"sequence": 0}))?;

    let probe = probe_descriptor(options, spec, &script);
    evidence.write_json(format!("{evidence_prefix}/probe.json"), &probe)?;
    let (descriptor, probe_error) = match probe {
        Ok(descriptor) => (Some(descriptor), None),
        Err(error) => (None, Some(error)),
    };
    let descriptor_valid = descriptor
        .as_ref()
        .is_some_and(|descriptor| descriptor.validate().is_ok());
    let mut result = ToolkitPreflight {
        toolkit: spec.toolkit,
        display_route: spec.display_route,
        descriptor_valid,
        runtime_backend: None,
        graphical_launch_attempted: false,
        final_consumer_ready: false,
        event_log_valid: false,
        event_count: 0,
        event_log_sha256: None,
        compositor_window_seen: false,
        x11_root_window_seen: (spec.display_route == DisplayRoute::Xwayland).then_some(false),
        cleanup_complete: true,
        limitation: probe_error,
        evidence: vec![
            "identity.json".to_owned(),
            format!("{evidence_prefix}/probe.json"),
        ],
    };

    if !descriptor_valid || options.probe_only {
        if options.probe_only && result.limitation.is_none() {
            result.limitation = Some("graphical launch was explicitly disabled".to_owned());
        }
        evidence.write_json(format!("{evidence_prefix}/result.json"), &result)?;
        result
            .evidence
            .push(format!("{evidence_prefix}/result.json"));
        return Ok(result);
    }

    if let Some(missing) = missing_display_prerequisite(spec.display_route) {
        result.limitation = Some(missing);
        evidence.write_json(format!("{evidence_prefix}/result.json"), &result)?;
        result
            .evidence
            .push(format!("{evidence_prefix}/result.json"));
        return Ok(result);
    }

    result.graphical_launch_attempted = true;
    let mut child = match launch_fixture(options, spec, &script, &state, &events, &control) {
        Ok(child) => ChildGuard(child),
        Err(error) => {
            result.limitation = Some(format!("fixture launch failed: {error}"));
            evidence.write_json(format!("{evidence_prefix}/result.json"), &result)?;
            result
                .evidence
                .push(format!("{evidence_prefix}/result.json"));
            return Ok(result);
        }
    };

    match wait_for_state(&state, Duration::from_secs(8)) {
        Ok(fixture_state) => {
            let expected = descriptor
                .as_ref()
                .expect("validated descriptor is present");
            fixture_state.validate()?;
            if &fixture_state.descriptor != expected {
                result.limitation =
                    Some("live fixture descriptor differs from API probe".to_owned());
            } else if fixture_state.ready {
                result.runtime_backend = Some(fixture_state.runtime_backend.clone());
                result.final_consumer_ready = true;
                result.compositor_window_seen =
                    wait_for_niri_window(&fixture_state.descriptor.title, Duration::from_secs(2));
                if spec.display_route == DisplayRoute::Xwayland {
                    result.x11_root_window_seen = Some(wait_for_x11_window(
                        &fixture_state.descriptor.title,
                        Duration::from_secs(2),
                    ));
                }
                evidence.write_json(format!("{evidence_prefix}/state.json"), &fixture_state)?;
                result
                    .evidence
                    .push(format!("{evidence_prefix}/state.json"));
            }
        }
        Err(error) => result.limitation = Some(error),
    }

    scratch.write_json(&control_relative, &json!({"sequence": 1, "action": "quit"}))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if child.0.try_wait()?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if child.0.try_wait()?.is_none() {
        child.terminate();
    }
    result.cleanup_complete = child.0.try_wait()?.is_some();
    match fs::read_to_string(&events) {
        Ok(contents) => match complete_fixture_events(&contents) {
            Ok(parsed_events) => {
                result.event_log_valid = true;
                result.event_count = parsed_events.len() as u64;
                result.event_log_sha256 = Some(sha256_file(&events)?);
            }
            Err(error) => {
                result.limitation = Some(format!("fixture event log is invalid: {error}"));
            }
        },
        Err(error) => {
            result.limitation = Some(format!("fixture event log could not be read: {error}"));
        }
    }
    if result.final_consumer_ready && !result.compositor_window_seen {
        result.limitation =
            Some("fixture became ready but no matching Niri window was observed".to_owned());
    }
    if spec.display_route == DisplayRoute::Xwayland
        && result.runtime_backend.as_deref() != Some("xcb")
    {
        result.limitation = Some(format!(
            "the XWayland fixture used runtime backend {:?} instead of xcb",
            result.runtime_backend
        ));
    }
    evidence.write_json(format!("{evidence_prefix}/result.json"), &result)?;
    result
        .evidence
        .push(format!("{evidence_prefix}/result.json"));
    Ok(result)
}

fn probe_descriptor(
    options: &Options,
    spec: FixtureSpec,
    script: &Path,
) -> Result<FixtureDescriptor, String> {
    let mut command = runner_command(options, spec.runner, script);
    command.args(["--probe-api", "--seed", &options.seed.to_string()]);
    if spec.display_route == DisplayRoute::Xwayland {
        command.args(["--display-route", "xwayland"]);
    }
    let output = command
        .output()
        .map_err(|error| format!("could not run API probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "API probe exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| format!("invalid probe JSON: {error}"))
}

fn launch_fixture(
    options: &Options,
    spec: FixtureSpec,
    script: &Path,
    state: &Path,
    events: &Path,
    control: &Path,
) -> Result<Child, std::io::Error> {
    let mut command = runner_command(options, spec.runner, script);
    command
        .args(["--state", state.to_string_lossy().as_ref()])
        .args(["--events", events.to_string_lossy().as_ref()])
        .args(["--control", control.to_string_lossy().as_ref()])
        .args(["--seed", &options.seed.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if spec.display_route == DisplayRoute::NativeWayland && spec.toolkit == FixtureToolkit::Gtk {
        command.env("GDK_BACKEND", "wayland");
    }
    if spec.display_route == DisplayRoute::Xwayland {
        command.args(["--display-route", "xwayland"]);
    }
    command.spawn()
}

fn runner_command(options: &Options, runner: Runner, script: &Path) -> Command {
    let mut command = match runner {
        Runner::Python => {
            let mut command = Command::new(&options.python);
            command.env("PYTHONDONTWRITEBYTECODE", "1");
            command
        }
        Runner::Electron => Command::new(&options.electron),
    };
    command.arg(script);
    command
}

fn wait_for_state(path: &Path, timeout: Duration) -> Result<FixtureState, String> {
    crate::fixture_poll::poll_json(
        path,
        timeout,
        Duration::from_millis(25),
        |state: &FixtureState| state.ready,
    )
}

fn wait_for_niri_window(title: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if niri_window_seen(title) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn niri_window_seen(title: &str) -> bool {
    let Ok(mut socket) = Socket::connect() else {
        return false;
    };
    let Ok(Ok(Response::Windows(windows))) = socket.send(Request::Windows) else {
        return false;
    };
    windows
        .iter()
        .filter(|window| window.title.as_deref() == Some(title))
        .count()
        == 1
}

fn wait_for_x11_window(title: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if x11_window_seen(title) {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    false
}

fn x11_window_seen(title: &str) -> bool {
    let Ok(root) = Command::new("xprop")
        .args(["-root", "_NET_CLIENT_LIST"])
        .output()
    else {
        return false;
    };
    let root_list_contains_title = root.status.success()
        && String::from_utf8_lossy(&root.stdout)
            .split(|character: char| character == ',' || character.is_whitespace())
            .filter(|value| value.starts_with("0x"))
            .any(|window| {
                Command::new("xprop")
                    .args(["-id", window, "_NET_WM_NAME", "WM_NAME"])
                    .output()
                    .is_ok_and(|properties| {
                        properties.status.success()
                            && String::from_utf8_lossy(&properties.stdout).contains(title)
                    })
            });
    if root_list_contains_title {
        return true;
    }
    Command::new("xwininfo")
        .args(["-root", "-tree"])
        .output()
        .is_ok_and(|tree| {
            tree.status.success() && String::from_utf8_lossy(&tree.stdout).contains(title)
        })
}

fn matrix_cells(
    toolkits: &[ToolkitPreflight],
    topology: Option<&TopologySnapshot>,
    keyboard_layouts: Option<&[String]>,
) -> Vec<MatrixCellAdmission> {
    let mut cells = toolkits
        .iter()
        .map(|toolkit| {
            let available = toolkit.descriptor_valid
                && toolkit.final_consumer_ready
                && toolkit.event_log_valid
                && toolkit.compositor_window_seen
                && toolkit.cleanup_complete
                && (toolkit.display_route != DisplayRoute::Xwayland
                    || toolkit.runtime_backend.as_deref() == Some("xcb"));
            MatrixCellAdmission {
                dimension: "toolkit".to_owned(),
                value: format!("{:?}", toolkit.toolkit).to_lowercase(),
                outcome: if available {
                    AdmissionOutcome::Available
                } else {
                    AdmissionOutcome::Unavailable
                },
                reason: (!available).then(|| {
                    toolkit.limitation.clone().unwrap_or_else(|| {
                        "fixture preflight did not reach every oracle".to_owned()
                    })
                }),
                evidence: toolkit.evidence.clone(),
            }
        })
        .collect::<Vec<_>>();

    let logical = topology
        .map(|topology| {
            topology
                .outputs
                .iter()
                .filter_map(|output| output.logical)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let topology_evidence = vec!["topology.json".to_owned()];
    cells.push(admission(
        "output_count",
        "two_or_more",
        logical.len() >= 2,
        "fewer than two active logical outputs were discovered",
        &topology_evidence,
    ));
    cells.push(admission(
        "scale",
        "1.0",
        logical
            .iter()
            .any(|output| (output.scale - 1.0).abs() < 0.001),
        "no scale-1.0 output was discovered",
        &topology_evidence,
    ));
    cells.push(admission(
        "scale",
        "fractional",
        logical
            .iter()
            .any(|output| (output.scale - output.scale.round()).abs() >= 0.001),
        "no fractional-scale output is present in the admitted topology",
        &topology_evidence,
    ));
    cells.push(admission(
        "scale",
        "2.0",
        logical
            .iter()
            .any(|output| (output.scale - 2.0).abs() < 0.001),
        "no scale-2.0 output is present in the admitted topology",
        &topology_evidence,
    ));
    cells.push(admission(
        "transform",
        "rotated_or_flipped",
        logical
            .iter()
            .any(|output| output.transform != TopologyTransform::Normal),
        "all admitted outputs use the normal transform",
        &topology_evidence,
    ));
    cells.push(admission(
        "topology",
        "negative_origin",
        logical.iter().any(|output| output.x < 0 || output.y < 0),
        "the admitted topology contains no negative logical origin",
        &topology_evidence,
    ));
    cells.push(admission(
        "topology",
        "gap",
        topology_has_gap(&logical),
        "the admitted topology contains no inter-output gap",
        &topology_evidence,
    ));

    let layout_evidence = vec!["keyboard-layouts.json".to_owned()];
    cells.push(admission(
        "keyboard_layout",
        "second_layout",
        keyboard_layouts.is_some_and(|layouts| layouts.len() >= 2),
        "Niri reports fewer than two configured keyboard layouts",
        &layout_evidence,
    ));

    for (dimension, value, environment_name, reason) in [
        (
            "layout_change",
            "hot_plug",
            "WALDO_VERIFICATION_SAFE_HOTPLUG",
            "no safely authorised disposable hot-plug provider is configured",
        ),
        (
            "clipboard",
            "manager_present",
            "WALDO_VERIFICATION_CLIPBOARD_MANAGER",
            "no deterministic clipboard-manager fixture is configured",
        ),
        (
            "accessibility",
            "successful_focus",
            "WALDO_VERIFICATION_ATSPI_FOCUS_FIXTURE",
            "no deterministic consumer that accepts AT-SPI Focus is configured",
        ),
        (
            "lifecycle",
            "disruptive_session",
            "WALDO_VERIFICATION_DISPOSABLE_SESSION",
            "no safely authorised disposable desktop session is configured",
        ),
    ] {
        cells.push(admission(
            dimension,
            value,
            declared_available(environment_name),
            reason,
            &["environment.json".to_owned()],
        ));
    }
    cells
}

fn scenario_readiness(
    toolkits: &[ToolkitPreflight],
    topology_available: bool,
) -> Result<ScenarioReadinessRecord, Box<dyn std::error::Error>> {
    let available = toolkits
        .iter()
        .filter(|toolkit| {
            toolkit.descriptor_valid
                && toolkit.final_consumer_ready
                && toolkit.event_log_valid
                && toolkit.compositor_window_seen
                && toolkit.cleanup_complete
                && (toolkit.display_route != DisplayRoute::Xwayland
                    || toolkit.runtime_backend.as_deref() == Some("xcb"))
        })
        .map(|toolkit| toolkit.toolkit)
        .collect::<std::collections::BTreeSet<_>>();
    let any_fixture = !available.is_empty();
    let all_toolkits = [
        FixtureToolkit::Gtk,
        FixtureToolkit::Qt,
        FixtureToolkit::Electron,
        FixtureToolkit::Xwayland,
    ]
    .into_iter()
    .all(|toolkit| available.contains(&toolkit));
    let common_blockers = vec![CAMPAIGN_BLOCKER.to_owned()];

    let scenarios = vec![
        scenario(
            "pointer-consumers",
            &[
                "pointer move",
                "buttons",
                "click count",
                "drag",
                "path",
                "scroll",
            ],
            &["fixture event JSONL", "fixture counters"],
            any_fixture,
            "no toolkit fixture completed its live preflight",
            &common_blockers,
        ),
        scenario(
            "keyboard-and-text-consumers",
            &["key press/release", "key chord", "virtual-keyboard text"],
            &[
                "physical-key event order",
                "logical-key digest",
                "text length and digest",
            ],
            any_fixture,
            "no toolkit fixture completed its live preflight",
            &common_blockers,
        ),
        scenario(
            "wait-and-capture-consumers",
            &["delayed predicate", "timeout", "capture"],
            &[
                "fixture phase and counters",
                "fixture visual marker",
                "process state",
            ],
            any_fixture,
            "no toolkit fixture completed its live preflight",
            &common_blockers,
        ),
        scenario(
            "window-and-workspace-consumers",
            &["window focus/move/resize/close", "workspace focus/move"],
            &[
                "Niri window/workspace state",
                "fixture identity and readiness state",
            ],
            any_fixture && topology_available,
            "a toolkit fixture or independent Niri topology is unavailable",
            &common_blockers,
        ),
        scenario(
            "boundary-target-consumers",
            &["image edges", "small targets", "model-image mapping"],
            &[
                "deterministic boundary targets",
                "fixture-local pointer coordinates",
            ],
            any_fixture,
            "no toolkit fixture completed its live preflight",
            &common_blockers,
        ),
        scenario(
            "coherent-mixed-batch-primitives",
            &[
                "pointer",
                "click or drag",
                "text or chord",
                "wait",
                "capture",
            ],
            &[
                "ordered fixture events",
                "fixture state digest",
                "visual marker",
            ],
            any_fixture,
            "no toolkit fixture completed its live preflight",
            &common_blockers,
        ),
        scenario(
            "mixed-toolkit-journey-primitives",
            &[
                "GTK segment",
                "Qt segment",
                "Electron segment",
                "XWayland segment",
            ],
            &[
                "four independent fixture states",
                "four toolkit event streams",
            ],
            all_toolkits,
            "not every GTK, Qt, Electron, and XWayland fixture completed preflight",
            &common_blockers,
        ),
    ];
    let record = ScenarioReadinessRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Provisional,
        generated_at: generated_at()?,
        scenarios,
    };
    record.validate()?;
    Ok(record)
}

fn scenario(
    id: &str,
    action_families: &[&str],
    final_consumers: &[&str],
    ready: bool,
    unavailable_reason: &str,
    blockers: &[String],
) -> ScenarioPrimitiveReadiness {
    ScenarioPrimitiveReadiness {
        id: id.to_owned(),
        action_families: action_families
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        final_consumers: final_consumers
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        outcome: if ready {
            PrimitiveOutcome::Ready
        } else {
            PrimitiveOutcome::Unavailable
        },
        reason: (!ready).then(|| unavailable_reason.to_owned()),
        evidence: vec![
            "matrix-admission.json".to_owned(),
            "identity.json".to_owned(),
        ],
        binding_dependent_finalisation: blockers.to_vec(),
    }
}

fn admission(
    dimension: &str,
    value: &str,
    available: bool,
    unavailable_reason: &str,
    evidence: &[String],
) -> MatrixCellAdmission {
    MatrixCellAdmission {
        dimension: dimension.to_owned(),
        value: value.to_owned(),
        outcome: if available {
            AdmissionOutcome::Available
        } else {
            AdmissionOutcome::Unavailable
        },
        reason: (!available).then(|| unavailable_reason.to_owned()),
        evidence: evidence.to_vec(),
    }
}

fn topology_has_gap(outputs: &[waldo_verification::TopologyRect]) -> bool {
    for (index, left) in outputs.iter().enumerate() {
        for right in outputs.iter().skip(index + 1) {
            let horizontal_overlap =
                left.y < right.y + right.height as i32 && right.y < left.y + left.height as i32;
            let vertical_overlap =
                left.x < right.x + right.width as i32 && right.x < left.x + left.width as i32;
            if horizontal_overlap {
                let gap = if left.x <= right.x {
                    right.x - (left.x + left.width as i32)
                } else {
                    left.x - (right.x + right.width as i32)
                };
                if gap > 0 {
                    return true;
                }
            }
            if vertical_overlap {
                let gap = if left.y <= right.y {
                    right.y - (left.y + left.height as i32)
                } else {
                    left.y - (right.y + right.height as i32)
                };
                if gap > 0 {
                    return true;
                }
            }
        }
    }
    false
}

fn capture_keyboard_layouts() -> Result<Vec<String>, String> {
    let mut socket = Socket::connect().map_err(|error| error.to_string())?;
    let response = socket
        .send(Request::KeyboardLayouts)
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    let Response::KeyboardLayouts(layouts) = response else {
        return Err("Niri returned an unexpected keyboard-layout response".to_owned());
    };
    Ok(layouts.names)
}

fn missing_display_prerequisite(route: DisplayRoute) -> Option<String> {
    match route {
        DisplayRoute::NativeWayland if std::env::var_os("WAYLAND_DISPLAY").is_none() => {
            Some("WAYLAND_DISPLAY is absent from the verifier environment".to_owned())
        }
        DisplayRoute::Xwayland if std::env::var_os("DISPLAY").is_none() => {
            Some("DISPLAY is absent from the verifier environment".to_owned())
        }
        _ => None,
    }
}

fn environment_evidence() -> serde_json::Value {
    let names = [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "NIRI_SOCKET",
        "DBUS_SESSION_BUS_ADDRESS",
        "WALDO_VERIFICATION_SAFE_HOTPLUG",
        "WALDO_VERIFICATION_CLIPBOARD_MANAGER",
        "WALDO_VERIFICATION_ATSPI_FOCUS_FIXTURE",
        "WALDO_VERIFICATION_DISPOSABLE_SESSION",
    ];
    let values = names
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                json!({"present": std::env::var_os(name).is_some()}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({"variables": values})
}

fn identity_evidence(options: &Options) -> Result<PreflightIdentity, Box<dyn std::error::Error>> {
    let commit = command_text(Command::new("git").args(["rev-parse", "HEAD"]))?;
    let verification_diff = Command::new("git")
        .args([
            "diff",
            "--binary",
            "HEAD",
            "--",
            "crates/waldo-verification",
        ])
        .output()?;
    if !verification_diff.status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&verification_diff.stderr).trim()
        )
        .into());
    }
    let verification_status = Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--",
            "crates/waldo-verification",
        ])
        .output()?;
    if !verification_status.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&verification_status.stderr).trim()
        )
        .into());
    }
    let python_path = command_text(
        Command::new(&options.python).args(["-c", "import sys; print(sys.executable)"]),
    )?;
    let current_exe = std::env::current_exe()?;
    let paths = [
        ("waldo-verification", current_exe),
        ("python", PathBuf::from(python_path)),
        (
            "electron",
            program_path(&options.electron, std::env::var_os("PATH").as_deref())?,
        ),
        (
            "interaction-common",
            options.fixture_directory.join("interaction_common.py"),
        ),
        (
            "interaction-gtk",
            options.fixture_directory.join("interaction_gtk.py"),
        ),
        (
            "interaction-qt",
            options.fixture_directory.join("interaction_qt.py"),
        ),
        (
            "interaction-electron-main",
            options.fixture_directory.join("interaction_electron.js"),
        ),
        (
            "interaction-electron-page",
            options.fixture_directory.join("interaction_electron.html"),
        ),
    ];
    let files = paths
        .into_iter()
        .map(|(name, path)| {
            Ok(FileIdentity {
                name: name.to_owned(),
                path: path.display().to_string(),
                sha256: sha256_file(&path)?,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    Ok(PreflightIdentity {
        commit,
        working_tree_dirty: !verification_status.stdout.is_empty(),
        verification_diff_sha256: sha256_bytes(&verification_diff.stdout)?,
        files,
    })
}

fn command_text(command: &mut Command) -> Result<String, Box<dyn std::error::Error>> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "command failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let value = String::from_utf8(output.stdout)?.trim().to_owned();
    if value.is_empty() {
        return Err("command returned an empty identity".into());
    }
    Ok(value)
}

/// The file a program name runs, found the way `Command` finds it: a name
/// containing a slash is used as given, and a bare name is searched for as an
/// executable file in each `PATH` directory in turn.
fn program_path(
    program: &Path,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    if program.components().count() > 1 {
        return Ok(program.to_owned());
    }
    std::env::split_paths(search_path.unwrap_or_default())
        .map(|directory| directory.join(program))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| format!("{} was not found on PATH", program.display()).into())
}

fn sha256_file(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("sha256sum").arg(path).output()?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum failed for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    parse_sha256(&output.stdout)
}

fn sha256_bytes(bytes: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("sha256sum stdin was unavailable")?
        .write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err("sha256sum failed for in-memory evidence".into());
    }
    parse_sha256(&output.stdout)
}

fn parse_sha256(output: &[u8]) -> Result<String, Box<dyn std::error::Error>> {
    let digest = String::from_utf8(output.to_vec())?
        .split_whitespace()
        .next()
        .ok_or("sha256sum returned no digest")?
        .to_owned();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("sha256sum returned an invalid digest".into());
    }
    Ok(digest)
}

fn declared_available(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )
    })
}

fn generated_at() -> Result<String, std::time::SystemTimeError> {
    Ok(format!(
        "unix-ns:{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use waldo_verification::TopologyRect;

    fn output(x: i32, y: i32, width: u32, height: u32) -> TopologyRect {
        TopologyRect {
            x,
            y,
            width,
            height,
            scale: 1.0,
            transform: TopologyTransform::Normal,
        }
    }

    #[test]
    fn program_path_searches_path_for_bare_names_and_keeps_explicit_paths() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "waldo-program-path-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let empty = root.join("empty");
        let bin = root.join("bin");
        fs::create_dir_all(&empty).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let not_executable = empty.join("electron");
        fs::write(&not_executable, "").unwrap();
        let electron = bin.join("electron");
        fs::write(&electron, "").unwrap();
        fs::set_permissions(&electron, fs::Permissions::from_mode(0o755)).unwrap();
        let search = std::env::join_paths([&empty, &bin]).unwrap();

        let found = program_path(Path::new("electron"), Some(&search));
        let explicit = program_path(Path::new("/opt/electron39/electron"), Some(&search));
        let missing = program_path(Path::new("electron"), Some(empty.as_os_str()));
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(found.unwrap(), electron);
        assert_eq!(explicit.unwrap(), Path::new("/opt/electron39/electron"));
        assert!(
            missing
                .unwrap_err()
                .to_string()
                .contains("electron was not found on PATH")
        );
    }

    #[test]
    fn gap_detection_distinguishes_contiguous_and_separated_outputs() {
        assert!(!topology_has_gap(&[
            output(0, 0, 100, 100),
            output(100, 0, 100, 100),
        ]));
        assert!(topology_has_gap(&[
            output(0, 0, 100, 100),
            output(120, 0, 100, 100),
        ]));
    }
}
