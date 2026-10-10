use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    CoordinateMapping, ElementHandle, LogicalRect, ObservationId, OutputId, SourceSelector,
    WindowId,
};

/// How the compositor pixels behind a requested observation were obtained.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureRoute {
    FullOutput,
    OutputRegion,
    /// A visible floating window represented by its exact region in the output image.
    VisibleWindowCrop,
}

/// Honest provenance for the image bytes in an observation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CaptureProvenance {
    pub route: CaptureRoute,
    /// The output-local logical rectangle supplied to screencopy, when cropped.
    pub output_local_bounds: Option<LogicalRect>,
}

/// Observable relationship between a continued observation and its predecessor.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ObservationContinuation {
    pub from: ObservationId,
    pub frame_changed: bool,
    pub geometry_changed: bool,
    pub focus_changed: bool,
}

/// An image carried inline across the controller boundary.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ImageData {
    pub mime_type: String,
    pub data_base64: String,
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
    pub capture: CaptureProvenance,
    pub image: ImageData,
    pub coordinates: CoordinateMapping,
    pub focus: FocusState,
    pub accessibility: Vec<AccessibilityNode>,
    pub accessibility_available: bool,
    /// Why requested accessibility evidence is missing or incomplete, when
    /// the semantic backend failed rather than being unsupported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_limitation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ObservationContinuation>,
}
