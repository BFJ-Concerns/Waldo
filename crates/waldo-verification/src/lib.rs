//! Deterministic fixtures and evidence records for Waldo's consumer journeys.

mod conformance;
mod fixture;
mod private_root;
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
pub use fixture::{
    BoundaryTargets, FixturePoint, FixtureRect, SeededSequence, TargetGrid, TargetGridError,
};
pub use private_root::{
    PrivacyLeak, PrivacyScan, PrivateRoot, PrivateRootError, publish_private_json,
    scan_private_root,
};
pub use run::{
    BinaryIdentity, CellOutcome, CellResult, CleanupCheck, CleanupOutcome, EvidenceArtifact,
    EvidenceDigest, EvidenceKind, FixtureIdentity, RunRecord, RunRecordError,
    SensitiveEvidenceManifest, VerificationIdentity, is_sha256_digest,
};
pub use topology::{
    TopologyError, TopologyOutput, TopologyPixelSize, TopologyRect, TopologySnapshot,
    TopologyTransform, capture_niri_topology,
};
