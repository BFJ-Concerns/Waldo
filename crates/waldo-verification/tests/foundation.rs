use std::os::unix::fs::PermissionsExt;

use waldo_verification::{
    AdmissionOutcome, BinaryIdentity, BoundaryTargets, CONFORMANCE_SCHEMA_VERSION, CampaignCell,
    CampaignOutcome, CellOutcome, CellResult, CleanupCheck, CleanupOutcome, DisplayRoute,
    EvidenceArtifact, EvidenceDigest, EvidenceKind, EvidenceMaturity, FinalCampaignRecord,
    FixtureCapabilities, FixtureCounters, FixtureDescriptor, FixtureIdentity, FixtureRect,
    FixtureState, FixtureToolkit, MatrixAdmissionRecord, MatrixCellAdmission, PrimitiveOutcome,
    PrivateRoot, RunRecord, ScenarioPrimitiveReadiness, ScenarioReadinessRecord, SeededSequence,
    SensitiveEvidenceManifest, TargetGrid, VerificationIdentity, complete_fixture_events,
};

fn digest(value: &str) -> EvidenceDigest {
    EvidenceDigest {
        algorithm: "sha256".to_owned(),
        value: value.to_owned(),
    }
}

fn valid_record() -> RunRecord {
    RunRecord {
        schema_version: 1,
        run_id: "2026-07-15T13-30-00+01-00-foundation".to_owned(),
        started_at: "2026-07-15T13:30:00+01:00".to_owned(),
        identity: VerificationIdentity {
            engine_model: "codex-gpt-5".to_owned(),
            waldo_commit: "1e689cee02a12380f40c74ed05f38f4fff65f95d".to_owned(),
            binaries: vec![BinaryIdentity {
                name: "waldod".to_owned(),
                path: "/tmp/waldo-run/bin/waldod".to_owned(),
                digest: digest("binary-digest"),
            }],
            plugin_manifest_version: "0.1.0".to_owned(),
        },
        fixture: FixtureIdentity {
            name: "target-grid".to_owned(),
            version: "1".to_owned(),
            seed: 42,
        },
        environment_cells: vec![CellResult {
            name: "normal-output".to_owned(),
            outcome: CellOutcome::Passed,
            reason: None,
            evidence: vec!["topology".to_owned()],
        }],
        evidence: vec![EvidenceArtifact {
            id: "topology".to_owned(),
            kind: EvidenceKind::IndependentOracle,
            location: "/tmp/waldo-run/topology.json".to_owned(),
            digest: Some(digest("topology-digest")),
        }],
        cleanup: vec![CleanupCheck {
            name: "driving-lease".to_owned(),
            outcome: CleanupOutcome::Unchanged,
            reason: None,
            evidence: vec!["topology".to_owned()],
        }],
        sensitive_evidence: SensitiveEvidenceManifest {
            excluded: vec!["clipboard payload".to_owned()],
            retained_locally: Vec::new(),
            redacted: Vec::new(),
        },
    }
}

#[test]
fn run_record_round_trips_and_validates() {
    let record = valid_record();
    record.validate().unwrap();
    let json = serde_json::to_string_pretty(&record).unwrap();
    let decoded: RunRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn run_record_requires_explicit_unavailable_reason() {
    let mut record = valid_record();
    record.environment_cells[0].outcome = CellOutcome::Unavailable;
    let error = record.validate().unwrap_err();
    assert!(error.to_string().contains("requires a reason"));
}

#[test]
fn run_record_requires_explicit_failure_reason() {
    let mut record = valid_record();
    record.environment_cells[0].outcome = CellOutcome::Failed;
    let error = record.validate().unwrap_err();
    assert!(error.to_string().contains("requires a reason"));
}

#[test]
fn seeded_targets_are_stable_and_cover_every_cell() {
    let bounds = FixtureRect {
        x: 100,
        y: 200,
        width: 80,
        height: 60,
    };
    let first = TargetGrid::generate(bounds, 4, 3, 7).unwrap();
    let second = TargetGrid::generate(bounds, 4, 3, 7).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.targets.len(), 12);
    for (index, target) in first.targets.iter().enumerate() {
        let row = index as u32 / 4;
        let column = index as u32 % 4;
        assert!((100 + column * 20..100 + (column + 1) * 20).contains(&target.x));
        assert!((200 + row * 20..200 + (row + 1) * 20).contains(&target.y));
    }
    assert_eq!(BoundaryTargets::generate(bounds).unwrap().targets.len(), 9);
}

#[test]
fn seeded_sequence_changes_with_seed() {
    let mut first = SeededSequence::new(1);
    let mut second = SeededSequence::new(2);
    assert_ne!(first.next_u64(), second.next_u64());
}

fn descriptor(toolkit: FixtureToolkit, display_route: DisplayRoute) -> FixtureDescriptor {
    FixtureDescriptor {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        fixture_version: "1".to_owned(),
        toolkit,
        display_route,
        seed: 42,
        title: "Waldo interaction oracle 0000002a".to_owned(),
        capabilities: FixtureCapabilities {
            pointer: true,
            keyboard: true,
            text: true,
            scroll: true,
            drag: true,
            delayed_state: true,
            final_consumer_log: true,
        },
    }
}

#[test]
fn fixture_contract_validates_equivalent_toolkit_descriptors() {
    for (toolkit, route) in [
        (FixtureToolkit::Gtk, DisplayRoute::NativeWayland),
        (FixtureToolkit::Qt, DisplayRoute::NativeWayland),
        (FixtureToolkit::Electron, DisplayRoute::NativeWayland),
        (FixtureToolkit::Xwayland, DisplayRoute::Xwayland),
    ] {
        descriptor(toolkit, route).validate().unwrap();
    }
}

#[test]
fn fixture_state_retains_only_text_length_and_digest() {
    let state = FixtureState {
        descriptor: descriptor(FixtureToolkit::Gtk, DisplayRoute::NativeWayland),
        runtime_backend: "GdkWaylandDisplay".to_owned(),
        recorded_at_unix_ns: 1,
        sequence: 3,
        ready: true,
        phase: "ready".to_owned(),
        focused_control: Some("text-entry".to_owned()),
        text_length: 6,
        text_sha256: Some(
            "bef57ec7f53a6d40beb640a780a639c83bc29ac8a9816f1fc6c5c6dcd93c4721".to_owned(),
        ),
        selection: None,
        counters: FixtureCounters::default(),
        target_geometry: None,
    };
    state.validate().unwrap();
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("abcdef"));
    assert!(json.contains("text_sha256"));
}

#[test]
fn fixture_event_reader_tolerates_only_an_incomplete_tail() {
    let event = serde_json::json!({
        "schema_version": CONFORMANCE_SCHEMA_VERSION,
        "fixture_version": "1",
        "toolkit": "gtk",
        "display_route": "native_wayland",
        "seed": 42,
        "sequence": 1,
        "recorded_at_unix_ns": 1,
        "kind": "pointer_moved",
        "control": "interaction-surface",
        "x": 4.0,
        "y": 5.0,
        "button": null,
        "horizontal_delta": null,
        "vertical_delta": null,
        "physical_key": null,
        "logical_key_sha256": null,
        "modifiers": [],
        "text_length": null,
        "text_sha256": null
    });
    let complete = format!("{}\n", serde_json::to_string(&event).unwrap());
    assert_eq!(
        complete_fixture_events(&(complete.clone() + "{\"schema_version\":"))
            .unwrap()
            .len(),
        1
    );
    assert!(complete_fixture_events(&(complete + "{not-json}\n")).is_err());
}

#[test]
fn provisional_matrix_requires_an_explicit_finalisation_blocker() {
    let mut record = MatrixAdmissionRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Provisional,
        binding_shapes_settled: false,
        fixture_release: "interaction-fixtures-v1".to_owned(),
        generated_at: "2026-07-17T12:00:00+01:00".to_owned(),
        finalisation_blockers: Vec::new(),
        cells: vec![MatrixCellAdmission {
            dimension: "toolkit".to_owned(),
            value: "gtk".to_owned(),
            outcome: AdmissionOutcome::Available,
            reason: None,
            evidence: vec!["gtk-preflight.json".to_owned()],
        }],
    };
    assert!(
        record
            .validate()
            .unwrap_err()
            .to_string()
            .contains("finalisation blocker")
    );
    record.finalisation_blockers =
        vec!["installed binding and wire conformance has not landed".to_owned()];
    record.validate().unwrap();
}

#[test]
fn unavailable_matrix_cells_require_reasons_and_evidence() {
    let mut record = MatrixAdmissionRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Provisional,
        binding_shapes_settled: false,
        fixture_release: "interaction-fixtures-v1".to_owned(),
        generated_at: "2026-07-17T12:00:00+01:00".to_owned(),
        finalisation_blockers: vec!["binding shapes are unsettled".to_owned()],
        cells: vec![MatrixCellAdmission {
            dimension: "scale".to_owned(),
            value: "fractional".to_owned(),
            outcome: AdmissionOutcome::Unavailable,
            reason: None,
            evidence: vec!["topology.json".to_owned()],
        }],
    };
    assert!(record.validate().is_err());
    record.cells[0].reason = Some("no fractional-scale output is present".to_owned());
    record.validate().unwrap();
}

#[test]
fn final_campaign_requires_v2_consumers_and_reasons_for_every_non_pass() {
    let mut record = FinalCampaignRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Final,
        binding_shapes_settled: true,
        corpus_schema_major: 2,
        generated_at: "2026-07-17T12:00:00+01:00".to_owned(),
        cells: vec![CampaignCell {
            id: "gtk-pointer".to_owned(),
            action_families: vec!["pointer".to_owned()],
            outcome: CampaignOutcome::Unavailable,
            reason: None,
            final_consumers: vec!["GTK event journal".to_owned()],
            evidence: vec!["toolkits/gtk/events.json".to_owned()],
        }],
    };
    assert!(record.validate().is_err());
    record.cells[0].reason = Some("GTK is not installed".to_owned());
    record.validate().unwrap();
    record.corpus_schema_major = 3;
    assert!(record.validate().is_err());
}

#[test]
fn private_roots_are_fresh_owner_only_atomic_and_payload_scannable() {
    let parent = std::env::temp_dir().join(format!(
        "waldo-private-root-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&parent).unwrap();
    let retained_path = parent.join("evidence");
    let retained = PrivateRoot::create_retained(&retained_path).unwrap();
    retained
        .write_json(
            "safe/state.json",
            &serde_json::json!({"digest": "redacted"}),
        )
        .unwrap();
    let metadata = std::fs::metadata(retained_path.join("safe/state.json")).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert!(PrivateRoot::create_retained(&retained_path).is_err());
    assert!(
        retained
            .scan_for_payloads(&[("generated", b"private-value")])
            .unwrap()
            .leaks
            .is_empty()
    );
    std::fs::write(retained_path.join("unsafe.txt"), "private-value").unwrap();
    std::fs::set_permissions(
        retained_path.join("unsafe.txt"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let scan = retained
        .scan_for_payloads(&[("generated", b"private-value")])
        .unwrap();
    assert_eq!(scan.leaks.len(), 1);
    assert_eq!(scan.leaks[0].location, "unsafe.txt");
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn scratch_root_cleans_up_on_drop() {
    let path = {
        let scratch = PrivateRoot::create_scratch("waldo-private-root-test").unwrap();
        scratch
            .write_json("nested/value.json", &serde_json::json!({"ok": true}))
            .unwrap();
        scratch.path().to_path_buf()
    };
    assert!(!path.exists());
}

#[test]
fn retained_root_rejects_a_symbolic_link_parent() {
    use std::os::unix::fs::symlink;

    let parent = std::env::temp_dir().join(format!(
        "waldo-private-root-symlink-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let real = parent.join("real");
    let linked = parent.join("linked");
    std::fs::create_dir_all(&real).unwrap();
    symlink(&real, &linked).unwrap();
    assert!(PrivateRoot::create_retained(linked.join("evidence")).is_err());
    std::fs::remove_dir_all(parent).unwrap();
}

#[test]
fn privacy_scan_detects_base64_and_refuses_symbolic_links() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use std::os::unix::fs::symlink;

    let scratch = PrivateRoot::create_scratch("waldo-privacy-test").unwrap();
    let encoded = BASE64.encode(b"private-value");
    std::fs::write(scratch.path().join("encoded.txt"), encoded).unwrap();
    std::fs::set_permissions(
        scratch.path().join("encoded.txt"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let scan = scratch
        .scan_for_payloads(&[("generated", b"private-value")])
        .unwrap();
    assert_eq!(scan.leaks.len(), 1);
    assert_eq!(scan.leaks[0].encoding, "base64");

    symlink("encoded.txt", scratch.path().join("linked.txt")).unwrap();
    assert!(
        scratch
            .scan_for_payloads(&[("generated", b"private-value")])
            .is_err()
    );
}

#[test]
fn scenario_readiness_cannot_drop_binding_dependent_finalisation() {
    let mut record = ScenarioReadinessRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION,
        evidence_maturity: EvidenceMaturity::Provisional,
        generated_at: "unix-ns:1".to_owned(),
        scenarios: vec![ScenarioPrimitiveReadiness {
            id: "pointer-consumers".to_owned(),
            action_families: vec!["pointer".to_owned()],
            final_consumers: vec!["fixture event JSONL".to_owned()],
            outcome: PrimitiveOutcome::Ready,
            reason: None,
            evidence: vec!["matrix-admission.json".to_owned()],
            binding_dependent_finalisation: Vec::new(),
        }],
    };
    assert!(record.validate().is_err());
    record.scenarios[0].binding_dependent_finalisation =
        vec!["saved request document remains parked".to_owned()];
    record.validate().unwrap();
}
