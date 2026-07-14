use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{LogicalRect, OutputId, OutputTransform, PixelSize, WindowId, WorkspaceId};

/// Availability and limits for one controller backend.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct BackendCapability {
    pub available: bool,
    pub version: Option<String>,
    pub limitation: Option<String>,
}

impl BackendCapability {
    /// Describes a backend which the current controller cannot use.
    pub fn unavailable(limitation: impl Into<String>) -> Self {
        Self {
            available: false,
            version: None,
            limitation: Some(limitation.into()),
        }
    }
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

impl Capabilities {
    /// A truthful baseline for a controller with no desktop backends attached.
    pub fn unavailable() -> Self {
        let limitation = || BackendCapability::unavailable("backend not initialised");
        Self {
            screencopy: limitation(),
            virtual_pointer: limitation(),
            virtual_keyboard: limitation(),
            niri_ipc: limitation(),
            accessibility: limitation(),
            clipboard: limitation(),
            physical_input_detection: limitation(),
            driving_indicator: limitation(),
        }
    }
}

/// One output in the active Niri layout.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct OutputState {
    pub id: OutputId,
    pub name: String,
    pub logical_bounds: LogicalRect,
    pub pixel_size: Option<PixelSize>,
    pub scale: f64,
    pub transform: OutputTransform,
    pub focused: bool,
}

/// One positively identified Niri window.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WindowState {
    pub id: WindowId,
    pub title: Option<String>,
    pub application_id: Option<String>,
    /// Process which owns the window's Wayland connection, where Niri knows it.
    #[serde(default)]
    pub pid: Option<u32>,
    pub logical_bounds: Option<LogicalRect>,
    pub focused: bool,
    pub workspace_id: Option<WorkspaceId>,
    pub output_id: Option<OutputId>,
}

/// One workspace in the active Niri session.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub index: u8,
    pub name: Option<String>,
    pub output_id: Option<OutputId>,
    pub active: bool,
    pub focused: bool,
}

/// One Wayland global advertised by the active compositor.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProtocolAdvertisement {
    pub interface: String,
    pub version: u32,
}

/// Concrete environment facts gathered without enabling desktop mutation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CompatibilityProfile {
    pub waldo_version: String,
    pub niri_version: Option<String>,
    pub wayland_display: Option<String>,
    pub protocols: Vec<ProtocolAdvertisement>,
}
