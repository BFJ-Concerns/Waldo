use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    Action, Capabilities, ClientId, CompatibilityProfile, ConnectionId, DrivingSessionId,
    ErrorCode, Observation, ObservationId, OutputState, Predicate, ProtocolError, RequestId,
    SourceSelector, WindowState, WorkspaceState,
};

/// Parameters for a fresh or continued visual observation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveRequest {
    pub source: SourceSelector,
    pub continue_from: Option<ObservationId>,
    pub include_accessibility: bool,
}

/// One ordered, observation-grounded action batch.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActRequest {
    pub observation_id: ObservationId,
    pub actions: Vec<Action>,
    pub verification: Vec<Predicate>,
}

/// Selects optional detail for a read-only desktop-state query.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    pub include_windows: bool,
}

/// Requests cancellation and lease release for the caller's driving session.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StopRequest {}

/// Explicitly transfers the driving lease to another connected client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HandoverRequest {
    pub target: ConnectionId,
}

/// Messages accepted by the private controller socket.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        request_id: RequestId,
        schema_major: u16,
        client_id: ClientId,
    },
    Observe {
        request_id: RequestId,
        request: ObserveRequest,
    },
    Act {
        request_id: RequestId,
        request: ActRequest,
    },
    Query {
        request_id: RequestId,
        request: QueryRequest,
    },
    Stop {
        request_id: RequestId,
        request: StopRequest,
    },
    Handover {
        request_id: RequestId,
        request: HandoverRequest,
    },
}

impl ClientMessage {
    /// Correlates a response without depending on the operation variant.
    pub const fn request_id(&self) -> RequestId {
        match self {
            Self::Hello { request_id, .. }
            | Self::Observe { request_id, .. }
            | Self::Act { request_id, .. }
            | Self::Query { request_id, .. }
            | Self::Stop { request_id, .. }
            | Self::Handover { request_id, .. } => *request_id,
        }
    }
}

/// The backend route actually used for one action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionRoute {
    VirtualPointer,
    VirtualKeyboard,
    Accessibility,
    Clipboard,
    ClipboardPaste,
    NiriIpc,
    Screencopy,
    ControllerWait,
}

/// The disposition of one ordered action.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Executed,
    Failed,
    /// The backend may have acted, but its authoritative acknowledgement was lost.
    Indeterminate,
    NotExecuted,
}

/// Non-structural data returned by an action which produces a value.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionOutput {
    Clipboard {
        mime_type: String,
        data_base64: String,
    },
}

/// Why an action batch stopped before nominal completion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    ExecutionFailed,
    TargetLost,
    StaleObservation,
    GeometryChanged,
    FocusChanged,
    UserPreempted,
    WaitFailed,
    VerificationFailed,
    SettleTimeout,
    Cancelled,
}

/// Structural evidence for one action, excluding sensitive payloads.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActionReceipt {
    pub index: usize,
    pub status: ReceiptStatus,
    pub route: Option<ActionRoute>,
    pub error: Option<ProtocolError>,
    /// Mechanical outcome for a wait action; absent for other action kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate_result: Option<bool>,
    /// Value produced by this action, absent for actions which only mutate state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ActionOutput>,
}

/// Result of an ordered action batch and its automatic settled observation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ActResult {
    pub driving_session: DrivingSessionId,
    pub receipts: Vec<ActionReceipt>,
    /// First action known not to have run; skips an earlier indeterminate dispatch.
    pub first_not_executed: Option<usize>,
    pub stop_reason: StopReason,
    /// Fresh settled evidence, absent only when post-action capture itself failed.
    #[serde(default)]
    pub observation: Option<Observation>,
    /// Failure after ordered execution which could not be attached to one action receipt.
    #[serde(default)]
    pub completion_error: Option<ProtocolError>,
    pub verification: Vec<bool>,
}

/// Read-only controller and desktop state.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct QueryResult {
    pub capabilities: Capabilities,
    pub profile: CompatibilityProfile,
    pub outputs: Vec<OutputState>,
    pub workspaces: Vec<WorkspaceState>,
    pub windows: Vec<WindowState>,
    pub focused_window: Option<crate::WindowId>,
    pub keyboard_layouts: Vec<String>,
    pub active_keyboard_layout: Option<String>,
    pub driving_session: Option<DrivingSessionId>,
    pub driving_connection: Option<ConnectionId>,
}

/// Result of stopping the caller's driving session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct StopResult {
    pub released_session: Option<DrivingSessionId>,
}

/// Result of an explicit lease handover.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct HandoverResult {
    pub previous_session: DrivingSessionId,
    pub driving_session: DrivingSessionId,
    pub owner: ConnectionId,
}

/// Messages returned by the private controller socket.
///
/// Result structures intentionally accept unknown fields so an older binding
/// can ignore additive data from a newer controller within this major version.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Connected {
        request_id: RequestId,
        connection_id: ConnectionId,
        capabilities: Capabilities,
    },
    Observed {
        request_id: RequestId,
        observation: Observation,
    },
    Acted {
        request_id: RequestId,
        result: ActResult,
    },
    Queried {
        request_id: RequestId,
        result: Box<QueryResult>,
    },
    Stopped {
        request_id: RequestId,
        result: StopResult,
    },
    HandedOver {
        request_id: RequestId,
        result: HandoverResult,
    },
    Error {
        request_id: Option<RequestId>,
        error: ProtocolError,
    },
}

impl ServerMessage {
    /// Correlates this response with a client request when one was decoded.
    pub const fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Connected { request_id, .. }
            | Self::Observed { request_id, .. }
            | Self::Acted { request_id, .. }
            | Self::Queried { request_id, .. }
            | Self::Stopped { request_id, .. }
            | Self::HandedOver { request_id, .. } => Some(*request_id),
            Self::Error { request_id, .. } => *request_id,
        }
    }
}

impl ProtocolError {
    /// Constructs the error returned when a client uses another major schema.
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
