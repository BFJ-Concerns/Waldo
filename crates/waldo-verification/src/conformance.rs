use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

use crate::run::is_sha256_digest;

pub const CONFORMANCE_SCHEMA_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceMaturity {
    Provisional,
    Final,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureToolkit {
    Gtk,
    Qt,
    Electron,
    Xwayland,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayRoute {
    NativeWayland,
    Xwayland,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCapabilities {
    pub pointer: bool,
    pub keyboard: bool,
    pub text: bool,
    pub scroll: bool,
    pub drag: bool,
    pub delayed_state: bool,
    pub final_consumer_log: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureDescriptor {
    pub schema_version: u16,
    pub fixture_version: String,
    pub toolkit: FixtureToolkit,
    pub display_route: DisplayRoute,
    pub seed: u64,
    pub title: String,
    pub capabilities: FixtureCapabilities,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureCounters {
    pub pointer_moves: u64,
    pub pointer_presses: u64,
    pub pointer_releases: u64,
    /// Pointer releases on the interaction surface that did not end a drag.
    pub clicks: u64,
    /// Pointer releases on the text entry.
    pub entry_clicks: u64,
    pub drags: u64,
    pub horizontal_scrolls: u64,
    pub vertical_scrolls: u64,
    pub key_presses: u64,
    pub key_releases: u64,
    pub text_changes: u64,
    pub delayed_completions: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureTargetRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureTargetGeometry {
    pub screen_x: Option<f64>,
    pub screen_y: Option<f64>,
    pub surface: FixtureTargetRect,
    pub entry: FixtureTargetRect,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureState {
    pub descriptor: FixtureDescriptor,
    pub runtime_backend: String,
    pub recorded_at_unix_ns: u64,
    pub sequence: u64,
    pub ready: bool,
    pub phase: String,
    pub focused_control: Option<String>,
    pub text_length: u64,
    pub text_sha256: Option<String>,
    pub selection: Option<String>,
    pub counters: FixtureCounters,
    #[serde(default)]
    pub target_geometry: Option<FixtureTargetGeometry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureEvent {
    pub schema_version: u16,
    pub fixture_version: String,
    pub toolkit: FixtureToolkit,
    pub display_route: DisplayRoute,
    pub seed: u64,
    pub sequence: u64,
    pub recorded_at_unix_ns: u64,
    pub kind: String,
    pub control: String,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub button: Option<u32>,
    pub horizontal_delta: Option<f64>,
    pub vertical_delta: Option<f64>,
    pub physical_key: Option<String>,
    pub logical_key_sha256: Option<String>,
    pub modifiers: Vec<String>,
    pub text_length: Option<u64>,
    pub text_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOutcome {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixCellAdmission {
    pub dimension: String,
    pub value: String,
    pub outcome: AdmissionOutcome,
    pub reason: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixAdmissionRecord {
    pub schema_version: u16,
    pub evidence_maturity: EvidenceMaturity,
    pub binding_shapes_settled: bool,
    pub fixture_release: String,
    pub generated_at: String,
    pub finalisation_blockers: Vec<String>,
    pub cells: Vec<MatrixCellAdmission>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimitiveOutcome {
    Ready,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignOutcome {
    Passed,
    Failed,
    Inconclusive,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignCell {
    pub id: String,
    pub action_families: Vec<String>,
    pub outcome: CampaignOutcome,
    pub reason: Option<String>,
    pub final_consumers: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FinalCampaignRecord {
    pub schema_version: u16,
    pub evidence_maturity: EvidenceMaturity,
    pub binding_shapes_settled: bool,
    pub corpus_schema_major: u16,
    pub generated_at: String,
    pub cells: Vec<CampaignCell>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioPrimitiveReadiness {
    pub id: String,
    pub action_families: Vec<String>,
    pub final_consumers: Vec<String>,
    pub outcome: PrimitiveOutcome,
    pub reason: Option<String>,
    pub evidence: Vec<String>,
    pub binding_dependent_finalisation: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioReadinessRecord {
    pub schema_version: u16,
    pub evidence_maturity: EvidenceMaturity,
    pub generated_at: String,
    pub scenarios: Vec<ScenarioPrimitiveReadiness>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConformanceRecordError(pub String);

impl fmt::Display for ConformanceRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ConformanceRecordError {}

impl FixtureDescriptor {
    pub fn validate(&self) -> Result<(), ConformanceRecordError> {
        validate_schema(self.schema_version)?;
        require_text("fixture_version", &self.fixture_version)?;
        require_text("title", &self.title)?;
        if self.display_route == DisplayRoute::Xwayland && self.toolkit != FixtureToolkit::Xwayland
        {
            return Err(ConformanceRecordError(
                "the XWayland display route requires the xwayland toolkit cell".to_owned(),
            ));
        }
        if !self.capabilities.final_consumer_log {
            return Err(ConformanceRecordError(
                "a conformance fixture must expose a final-consumer log".to_owned(),
            ));
        }
        Ok(())
    }
}

impl FixtureState {
    pub fn validate(&self) -> Result<(), ConformanceRecordError> {
        self.descriptor.validate()?;
        require_text("runtime_backend", &self.runtime_backend)?;
        require_text("phase", &self.phase)?;
        validate_optional_digest("text_sha256", self.text_sha256.as_deref())
    }
}

impl FixtureEvent {
    pub fn validate(&self) -> Result<(), ConformanceRecordError> {
        validate_schema(self.schema_version)?;
        require_text("fixture_version", &self.fixture_version)?;
        require_text("kind", &self.kind)?;
        require_text("control", &self.control)?;
        if self.display_route == DisplayRoute::Xwayland && self.toolkit != FixtureToolkit::Xwayland
        {
            return Err(ConformanceRecordError(
                "the XWayland display route requires the xwayland toolkit cell".to_owned(),
            ));
        }
        validate_optional_digest("logical_key_sha256", self.logical_key_sha256.as_deref())?;
        validate_optional_digest("text_sha256", self.text_sha256.as_deref())
    }
}

pub fn complete_fixture_events(
    contents: &str,
) -> Result<Vec<FixtureEvent>, ConformanceRecordError> {
    let mut events = Vec::new();
    let mut previous_sequence = None;
    for complete_line in contents.split_inclusive('\n') {
        if !complete_line.ends_with('\n') {
            continue;
        }
        let line = complete_line.trim();
        if line.is_empty() {
            continue;
        }
        let event: FixtureEvent = serde_json::from_str(line).map_err(|error| {
            ConformanceRecordError(format!("invalid complete fixture-event line: {error}"))
        })?;
        event.validate()?;
        if previous_sequence.is_some_and(|previous| event.sequence <= previous) {
            return Err(ConformanceRecordError(format!(
                "fixture event sequence {} does not follow {previous_sequence:?}",
                event.sequence
            )));
        }
        previous_sequence = Some(event.sequence);
        events.push(event);
    }
    Ok(events)
}

impl MatrixAdmissionRecord {
    pub fn validate(&self) -> Result<(), ConformanceRecordError> {
        validate_schema(self.schema_version)?;
        require_text("fixture_release", &self.fixture_release)?;
        require_text("generated_at", &self.generated_at)?;
        if self.cells.is_empty() {
            return Err(ConformanceRecordError(
                "at least one matrix admission cell is required".to_owned(),
            ));
        }
        match self.evidence_maturity {
            EvidenceMaturity::Provisional => {
                if self.finalisation_blockers.is_empty() {
                    return Err(ConformanceRecordError(
                        "provisional evidence requires a finalisation blocker".to_owned(),
                    ));
                }
                if self.binding_shapes_settled {
                    return Err(ConformanceRecordError(
                        "provisional evidence cannot claim settled binding shapes".to_owned(),
                    ));
                }
            }
            EvidenceMaturity::Final => {
                if !self.binding_shapes_settled || !self.finalisation_blockers.is_empty() {
                    return Err(ConformanceRecordError(
                        "final evidence requires settled binding shapes and no blockers".to_owned(),
                    ));
                }
            }
        }

        let mut identities = BTreeSet::new();
        for cell in &self.cells {
            require_text("cell.dimension", &cell.dimension)?;
            require_text("cell.value", &cell.value)?;
            if !identities.insert((&cell.dimension, &cell.value)) {
                return Err(ConformanceRecordError(format!(
                    "duplicate matrix cell {}:{}",
                    cell.dimension, cell.value
                )));
            }
            if cell.evidence.is_empty() {
                return Err(ConformanceRecordError(format!(
                    "matrix cell {}:{} requires evidence",
                    cell.dimension, cell.value
                )));
            }
            if cell.outcome == AdmissionOutcome::Unavailable
                && cell
                    .reason
                    .as_ref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(ConformanceRecordError(format!(
                    "unavailable matrix cell {}:{} requires a reason",
                    cell.dimension, cell.value
                )));
            }
        }
        Ok(())
    }
}

impl ScenarioReadinessRecord {
    pub fn validate(&self) -> Result<(), ConformanceRecordError> {
        validate_schema(self.schema_version)?;
        require_text("generated_at", &self.generated_at)?;
        if self.evidence_maturity != EvidenceMaturity::Provisional {
            return Err(ConformanceRecordError(
                "fixture-primitive readiness must remain provisional".to_owned(),
            ));
        }
        if self.scenarios.is_empty() {
            return Err(ConformanceRecordError(
                "at least one fixture-primitive scenario is required".to_owned(),
            ));
        }
        let mut identifiers = BTreeSet::new();
        for scenario in &self.scenarios {
            require_text("scenario.id", &scenario.id)?;
            if !identifiers.insert(&scenario.id) {
                return Err(ConformanceRecordError(format!(
                    "duplicate fixture-primitive scenario {}",
                    scenario.id
                )));
            }
            if scenario.action_families.is_empty()
                || scenario.final_consumers.is_empty()
                || scenario.evidence.is_empty()
                || scenario.binding_dependent_finalisation.is_empty()
            {
                return Err(ConformanceRecordError(format!(
                    "scenario {} requires action families, final consumers, evidence, and binding finalisation blockers",
                    scenario.id
                )));
            }
            if scenario.outcome == PrimitiveOutcome::Unavailable
                && scenario
                    .reason
                    .as_ref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(ConformanceRecordError(format!(
                    "unavailable scenario {} requires a reason",
                    scenario.id
                )));
            }
        }
        Ok(())
    }
}

impl FinalCampaignRecord {
    pub fn validate(&self) -> Result<(), ConformanceRecordError> {
        validate_schema(self.schema_version)?;
        require_text("generated_at", &self.generated_at)?;
        if self.evidence_maturity != EvidenceMaturity::Final || !self.binding_shapes_settled {
            return Err(ConformanceRecordError(
                "a final campaign requires final evidence and settled binding shapes".to_owned(),
            ));
        }
        if self.corpus_schema_major != 2 {
            return Err(ConformanceRecordError(format!(
                "the final campaign must consume the v2 saved corpus, not schema major {}",
                self.corpus_schema_major
            )));
        }
        if self.cells.is_empty() {
            return Err(ConformanceRecordError(
                "a final campaign requires at least one coverage cell".to_owned(),
            ));
        }

        let mut identifiers = BTreeSet::new();
        for cell in &self.cells {
            require_text("campaign cell id", &cell.id)?;
            if !identifiers.insert(&cell.id) {
                return Err(ConformanceRecordError(format!(
                    "duplicate final campaign cell {}",
                    cell.id
                )));
            }
            if cell.action_families.is_empty()
                || cell.final_consumers.is_empty()
                || cell.evidence.is_empty()
            {
                return Err(ConformanceRecordError(format!(
                    "campaign cell {} requires action families, final consumers, and evidence",
                    cell.id
                )));
            }
            if cell.outcome != CampaignOutcome::Passed
                && cell
                    .reason
                    .as_ref()
                    .is_none_or(|reason| reason.trim().is_empty())
            {
                return Err(ConformanceRecordError(format!(
                    "non-passing campaign cell {} requires a reason",
                    cell.id
                )));
            }
        }
        Ok(())
    }
}

fn validate_schema(schema_version: u16) -> Result<(), ConformanceRecordError> {
    if schema_version != CONFORMANCE_SCHEMA_VERSION {
        return Err(ConformanceRecordError(format!(
            "unsupported conformance schema version {schema_version}"
        )));
    }
    Ok(())
}

fn validate_optional_digest(
    field: &str,
    digest: Option<&str>,
) -> Result<(), ConformanceRecordError> {
    if let Some(digest) = digest
        && !is_sha256_digest(digest)
    {
        return Err(ConformanceRecordError(format!(
            "{field} must be a 64-character hexadecimal SHA-256 digest"
        )));
    }
    Ok(())
}

fn require_text(field: &str, value: &str) -> Result<(), ConformanceRecordError> {
    if value.trim().is_empty() {
        return Err(ConformanceRecordError(format!("{field} cannot be empty")));
    }
    Ok(())
}
