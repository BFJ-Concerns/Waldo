use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

pub const RUN_RECORD_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceDigest {
    pub algorithm: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BinaryIdentity {
    pub name: String,
    pub path: String,
    pub digest: EvidenceDigest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationIdentity {
    pub engine_model: String,
    pub waldo_commit: String,
    pub binaries: Vec<BinaryIdentity>,
    pub plugin_manifest_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureIdentity {
    pub name: String,
    pub version: String,
    pub seed: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    CapabilitySnapshot,
    PluginInventory,
    BeforeObservation,
    CoordinateMapping,
    SavedRequest,
    ReceiptTimeline,
    FrameSamples,
    ReturnedObservation,
    LaterObservation,
    IndependentOracle,
    CleanupState,
    SensitiveEvidenceManifest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceArtifact {
    pub id: String,
    pub kind: EvidenceKind,
    pub location: String,
    pub digest: Option<EvidenceDigest>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CellOutcome {
    Passed,
    Failed,
    Inconclusive,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellResult {
    pub name: String,
    pub outcome: CellOutcome,
    pub reason: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    Restored,
    Unchanged,
    Failed,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupCheck {
    pub name: String,
    pub outcome: CleanupOutcome,
    pub reason: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SensitiveEvidenceManifest {
    pub excluded: Vec<String>,
    pub retained_locally: Vec<String>,
    pub redacted: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub schema_version: u16,
    pub run_id: String,
    pub started_at: String,
    pub identity: VerificationIdentity,
    pub fixture: FixtureIdentity,
    pub environment_cells: Vec<CellResult>,
    pub evidence: Vec<EvidenceArtifact>,
    pub cleanup: Vec<CleanupCheck>,
    pub sensitive_evidence: SensitiveEvidenceManifest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRecordError(pub String);

impl fmt::Display for RunRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RunRecordError {}

impl RunRecord {
    pub fn validate(&self) -> Result<(), RunRecordError> {
        if self.schema_version != RUN_RECORD_SCHEMA_VERSION {
            return Err(RunRecordError(format!(
                "unsupported run-record schema version {}",
                self.schema_version
            )));
        }
        require_text("run_id", &self.run_id)?;
        require_text("started_at", &self.started_at)?;
        require_text("engine_model", &self.identity.engine_model)?;
        validate_commit(&self.identity.waldo_commit)?;
        require_text(
            "plugin_manifest_version",
            &self.identity.plugin_manifest_version,
        )?;
        require_text("fixture.name", &self.fixture.name)?;
        require_text("fixture.version", &self.fixture.version)?;
        if self.identity.binaries.is_empty() {
            return Err(RunRecordError(
                "at least one tested binary identity is required".to_owned(),
            ));
        }

        let mut binary_names = BTreeSet::new();
        for binary in &self.identity.binaries {
            require_text("binary.name", &binary.name)?;
            require_text("binary.path", &binary.path)?;
            validate_digest(&binary.digest)?;
            if !binary_names.insert(&binary.name) {
                return Err(RunRecordError(format!(
                    "duplicate binary identity {}",
                    binary.name
                )));
            }
        }

        let mut evidence_ids = BTreeSet::new();
        for artifact in &self.evidence {
            require_text("evidence.id", &artifact.id)?;
            require_text("evidence.location", &artifact.location)?;
            if let Some(digest) = &artifact.digest {
                validate_digest(digest)?;
            }
            if !evidence_ids.insert(&artifact.id) {
                return Err(RunRecordError(format!(
                    "duplicate evidence id {}",
                    artifact.id
                )));
            }
        }

        let mut cell_names = BTreeSet::new();
        for cell in &self.environment_cells {
            require_text("environment_cell.name", &cell.name)?;
            if !cell_names.insert(&cell.name) {
                return Err(RunRecordError(format!(
                    "duplicate environment cell {}",
                    cell.name
                )));
            }
            validate_outcome(
                "environment cell",
                &cell.name,
                cell.outcome != CellOutcome::Passed,
                &cell.reason,
                &cell.evidence,
                &evidence_ids,
            )?;
        }

        let mut cleanup_names = BTreeSet::new();
        for check in &self.cleanup {
            require_text("cleanup.name", &check.name)?;
            if !cleanup_names.insert(&check.name) {
                return Err(RunRecordError(format!(
                    "duplicate cleanup check {}",
                    check.name
                )));
            }
            validate_outcome(
                "cleanup check",
                &check.name,
                matches!(
                    check.outcome,
                    CleanupOutcome::Failed | CleanupOutcome::Unavailable
                ),
                &check.reason,
                &check.evidence,
                &evidence_ids,
            )?;
        }
        Ok(())
    }
}

fn validate_outcome(
    category: &str,
    name: &str,
    reason_required: bool,
    reason: &Option<String>,
    evidence: &[String],
    evidence_ids: &BTreeSet<&String>,
) -> Result<(), RunRecordError> {
    if reason_required
        && reason
            .as_ref()
            .is_none_or(|reason| reason.trim().is_empty())
    {
        return Err(RunRecordError(format!(
            "{category} {name} requires a reason"
        )));
    }
    if evidence.is_empty() {
        return Err(RunRecordError(format!(
            "{category} {name} requires evidence"
        )));
    }
    for evidence_id in evidence {
        if !evidence_ids.contains(evidence_id) {
            return Err(RunRecordError(format!(
                "{category} {name} references unknown evidence {evidence_id}"
            )));
        }
    }
    Ok(())
}

fn validate_commit(commit: &str) -> Result<(), RunRecordError> {
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RunRecordError(
            "waldo_commit must be a full 40-character hexadecimal object id".to_owned(),
        ));
    }
    Ok(())
}

/// Whether `value` is a SHA-256 digest: 64 hexadecimal characters.
pub fn is_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_digest(digest: &EvidenceDigest) -> Result<(), RunRecordError> {
    if digest.algorithm != "sha256" {
        return Err(RunRecordError(format!(
            "digest.algorithm {:?} is not supported; records use sha256",
            digest.algorithm
        )));
    }
    if !is_sha256_digest(&digest.value) {
        return Err(RunRecordError(
            "digest.value must be 64 hexadecimal characters".to_owned(),
        ));
    }
    Ok(())
}

fn require_text(field: &str, value: &str) -> Result<(), RunRecordError> {
    if value.trim().is_empty() {
        return Err(RunRecordError(format!("{field} cannot be empty")));
    }
    Ok(())
}
