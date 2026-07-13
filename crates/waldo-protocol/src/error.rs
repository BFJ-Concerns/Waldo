use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Stable machine-readable failure categories across every binding.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    IncompatibleSchema,
    InvalidRequest,
    UnsupportedCapability,
    UnknownConnection,
    LeaseHeld,
    NotLeaseOwner,
    StaleObservation,
    StaleAccessibilityTarget,
    TargetLost,
    GeometryChanged,
    FocusChanged,
    UserPreempted,
    WaitFailed,
    VerificationFailed,
    SettleTimeout,
    Cancelled,
    BackendFailure,
    Internal,
}

/// A structured failure safe to return through CLI or MCP.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}
