//! Deterministic fixtures and evidence records for Waldo's consumer journeys.

mod conformance;
mod estate;
mod fixture;
mod run;
mod topology;

pub use conformance::{
    AdmissionOutcome, CONFORMANCE_SCHEMA_VERSION, CampaignCell, CampaignOutcome,
    ConformanceRecordError, DisplayRoute, EvidenceMaturity, FinalCampaignRecord,
    FixtureCapabilities, FixtureCounters, FixtureDescriptor, FixtureEvent, FixtureState,
    FixtureTargetGeometry, FixtureTargetRect, FixtureToolkit, MatrixAdmissionRecord,
    MatrixCellAdmission, PrimitiveOutcome, ScenarioPrimitiveReadiness, ScenarioReadinessRecord,
    complete_fixture_events,
};
pub use estate::{EstateError, PrivacyLeak, PrivacyScan, PrivateRoot, scan_private_root};
pub use fixture::{
    BoundaryTargets, FixturePoint, FixtureRect, SeededSequence, TargetGrid, TargetGridError,
};
pub use run::{
    BinaryIdentity, CellOutcome, CellResult, CleanupCheck, CleanupOutcome, EvidenceArtifact,
    EvidenceDigest, EvidenceKind, FixtureIdentity, RunRecord, RunRecordError,
    SensitiveEvidenceManifest, VerificationIdentity,
};
pub use topology::{
    TopologyError, TopologyOutput, TopologyPixelSize, TopologyRect, TopologySnapshot,
    TopologyTransform, capture_niri_topology,
};
