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
    AmbiguousTarget,
    TargetLost,
    GeometryChanged,
    FocusChanged,
    WaitFailed,
    VerificationFailed,
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

impl ProtocolError {
    /// A desktop backend or transport failed while serving the request;
    /// `retryable` says whether repeating the same request can succeed.
    pub fn backend_failure(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: ErrorCode::BackendFailure,
            message: message.into(),
            retryable,
        }
    }

    /// The request decoded but asks for something the contract refuses, so
    /// repeating it unchanged cannot succeed.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: message.into(),
            retryable: false,
        }
    }

    /// The capability is absent for this source, backend or session, as
    /// Query discloses; repeating the request cannot make it present.
    pub fn unsupported_capability(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::UnsupportedCapability,
            message: message.into(),
            retryable: false,
        }
    }

    /// The batch was cancelled before or during this action; the caller
    /// decides what to repeat after observing again.
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Cancelled,
            message: message.into(),
            retryable: false,
        }
    }

    /// One of Waldo's own invariants failed; never retryable.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::Internal,
            message: message.into(),
            retryable: false,
        }
    }

    /// The client speaks another major version of the schema.
    pub fn incompatible_schema(expected: u16, received: u16) -> Self {
        Self {
            code: ErrorCode::IncompatibleSchema,
            message: format!(
                "incompatible schema major version: controller uses {expected}, client sent {received}"
            ),
            retryable: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorCode, ProtocolError};

    #[test]
    fn constructors_fix_the_code_and_retryability_of_each_failure_class() {
        let cases = [
            (
                ProtocolError::backend_failure("flush failed", true),
                ErrorCode::BackendFailure,
                true,
            ),
            (
                ProtocolError::backend_failure("device gone", false),
                ErrorCode::BackendFailure,
                false,
            ),
            (
                ProtocolError::invalid_request("region outside the output"),
                ErrorCode::InvalidRequest,
                false,
            ),
            (
                ProtocolError::unsupported_capability("no window capture"),
                ErrorCode::UnsupportedCapability,
                false,
            ),
            (
                ProtocolError::cancelled("cancelled before dispatch"),
                ErrorCode::Cancelled,
                false,
            ),
            (
                ProtocolError::internal("state lock is poisoned"),
                ErrorCode::Internal,
                false,
            ),
            (
                ProtocolError::incompatible_schema(2, 1),
                ErrorCode::IncompatibleSchema,
                false,
            ),
        ];
        for (error, code, retryable) in cases {
            assert_eq!(error.code, code, "{}", error.message);
            assert_eq!(error.retryable, retryable, "{}", error.message);
        }
    }

    #[test]
    fn messages_pass_through_unchanged() {
        assert_eq!(
            ProtocolError::invalid_request(String::from("capture region must be whole pixels"))
                .message,
            "capture region must be whole pixels"
        );
        assert_eq!(
            ProtocolError::incompatible_schema(2, 3).message,
            "incompatible schema major version: controller uses 2, client sent 3"
        );
    }
}
