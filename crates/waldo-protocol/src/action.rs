use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ElementHandle, LogicalDelta, LogicalPoint, ObservationId, SourceSelector, WindowId, WorkspaceId,
};

/// A pointer button independent of Linux's numeric button codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
    Back,
    Forward,
}

/// Whether an explicitly held key or button is pressed or released.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    Pressed,
    Released,
}

/// Preference order for deterministic text entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRoutePreference {
    Automatic,
    Accessibility,
    ClipboardPaste,
    VirtualKeyboard,
}

/// One exact MIME representation within an atomic clipboard offer.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipboardRepresentation {
    pub mime_type: String,
    pub data_base64: String,
}

/// A deterministic accessibility operation against an opaque element handle.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessibilityOperation {
    Invoke { action: String },
    SetText { text: String },
    SetValue { value: String },
    Focus,
    Select,
}

/// Niri-native operations against a positively identified window.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WindowOperation {
    Focus,
    Close,
    MoveToWorkspace { workspace_id: WorkspaceId },
    MoveToOutput { output: String },
    SetSize { width: u32, height: u32 },
}

/// Niri-native operations against a positively identified workspace.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceOperation {
    Focus,
    MoveToOutput { output: String },
}

/// Mechanical conditions which may be waited for or verified.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    FrameChanged {
        from: ObservationId,
    },
    ElementExists {
        element: ElementHandle,
    },
    ElementState {
        element: ElementHandle,
        state: String,
        present: bool,
    },
    WindowExists {
        window_id: WindowId,
    },
    FocusedWindow {
        window_id: WindowId,
    },
    ProcessExited {
        pid: u32,
    },
}

/// One locally executed member of an ordered action batch.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    PointerMoveAbsolute {
        position: LogicalPoint,
    },
    PointerMoveRelative {
        delta: LogicalDelta,
    },
    PointerButton {
        button: PointerButton,
        state: InputState,
    },
    PointerClick {
        position: LogicalPoint,
        button: PointerButton,
        count: u8,
    },
    PointerDrag {
        from: LogicalPoint,
        to: LogicalPoint,
        button: PointerButton,
        duration_ms: u32,
    },
    PointerPath {
        points: Vec<LogicalPoint>,
        duration_ms: u32,
    },
    Scroll {
        delta: LogicalDelta,
    },
    Key {
        key: String,
        state: InputState,
    },
    KeyChord {
        keys: Vec<String>,
    },
    Text {
        text: String,
        route: TextRoutePreference,
    },
    Accessibility {
        element: ElementHandle,
        operation: AccessibilityOperation,
    },
    ClipboardRead {
        mime_type: String,
    },
    ClipboardWrite {
        mime_type: String,
        data_base64: String,
    },
    ClipboardWriteOffer {
        representations: Vec<ClipboardRepresentation>,
    },
    ClipboardPreserve,
    ClipboardRestore {
        preservation_id: crate::ClipboardPreservationId,
    },
    ClipboardKeep {
        preservation_id: crate::ClipboardPreservationId,
    },
    Window {
        window_id: WindowId,
        operation: WindowOperation,
    },
    Workspace {
        workspace_id: WorkspaceId,
        operation: WorkspaceOperation,
    },
    Wait {
        predicate: Predicate,
        timeout_ms: u32,
    },
    Capture {
        source: SourceSelector,
    },
}
