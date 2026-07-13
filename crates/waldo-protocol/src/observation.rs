use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    CoordinateMapping, ElementHandle, LogicalPoint, LogicalRect, ObservationId, OutputId,
    SourceSelector, WindowId,
};

/// An image carried inline across the controller boundary.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ImageData {
    pub mime_type: String,
    pub data_base64: String,
}

/// Cursor evidence associated with an observation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CursorState {
    pub visible: bool,
    pub position: Option<LogicalPoint>,
}

/// Session-scoped focus and target identity captured with a frame.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct FocusState {
    pub window_id: Option<WindowId>,
    pub element: Option<ElementHandle>,
}

/// Compact semantic evidence from AT-SPI.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct AccessibilityNode {
    pub handle: ElementHandle,
    pub application: String,
    pub window_id: Option<WindowId>,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub text: Option<String>,
    pub states: Vec<String>,
    pub bounds: Option<LogicalRect>,
    pub actions: Vec<String>,
    pub children: Vec<ElementHandle>,
}

/// A screenshot and the state required to ground actions against it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Observation {
    pub id: ObservationId,
    pub captured_at_unix_ms: u64,
    pub source: SourceSelector,
    pub stable_output_id: OutputId,
    pub image: ImageData,
    pub coordinates: CoordinateMapping,
    pub cursor: Option<CursorState>,
    pub focus: FocusState,
    pub accessibility: Vec<AccessibilityNode>,
    pub accessibility_available: bool,
}
