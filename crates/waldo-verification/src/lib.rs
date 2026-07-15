//! Deterministic fixtures and evidence records for Waldo's consumer journeys.

mod fixture;
mod run;
mod topology;

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
