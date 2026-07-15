use waldo_verification::{
    BinaryIdentity, BoundaryTargets, CellOutcome, CellResult, CleanupCheck, CleanupOutcome,
    EvidenceArtifact, EvidenceDigest, EvidenceKind, FixtureIdentity, FixtureRect, RunRecord,
    SeededSequence, SensitiveEvidenceManifest, TargetGrid, VerificationIdentity,
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
