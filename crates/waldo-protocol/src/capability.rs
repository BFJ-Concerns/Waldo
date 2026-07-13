use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{LogicalRect, OutputId, WindowId};

/// Availability and limits for one controller backend.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct BackendCapability {
    pub available: bool,
    pub version: Option<String>,
    pub limitation: Option<String>,
}

/// The concrete operations enabled in the active desktop session.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Capabilities {
    pub screencopy: BackendCapability,
    pub virtual_pointer: BackendCapability,
    pub virtual_keyboard: BackendCapability,
    pub niri_ipc: BackendCapability,
    pub accessibility: BackendCapability,
    pub clipboard: BackendCapability,
    pub physical_input_detection: BackendCapability,
    pub driving_indicator: BackendCapability,
}

/// One output in the active Niri layout.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct OutputState {
    pub id: OutputId,
    pub name: String,
    pub logical_bounds: LogicalRect,
    pub scale: f64,
    pub focused: bool,
}

/// One positively identified Niri window.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WindowState {
    pub id: WindowId,
    pub title: String,
    pub application_id: Option<String>,
    pub logical_bounds: LogicalRect,
    pub focused: bool,
    pub workspace: String,
    pub output_id: OutputId,
}
