use std::{collections::HashMap, env};

use niri_ipc::{KeyboardLayouts, Output, Request, Response, Window, Workspace, socket::Socket};
use waldo_protocol::{
    BackendCapability, Capabilities, CompatibilityProfile, OutputState, ProtocolAdvertisement,
    WindowId, WindowState, WorkspaceState,
};

use crate::{InspectionError, convert, wayland_registry};

/// A coherent-enough read-only snapshot for controller diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct NiriSnapshot {
    pub capabilities: Capabilities,
    pub profile: CompatibilityProfile,
    pub outputs: Vec<OutputState>,
    pub workspaces: Vec<WorkspaceState>,
    pub windows: Vec<WindowState>,
    pub focused_window: Option<WindowId>,
    pub keyboard_layouts: Vec<String>,
    pub active_keyboard_layout: Option<String>,
}

impl NiriSnapshot {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let mut capabilities = Capabilities::unavailable();
        capabilities.niri_ipc = BackendCapability::unavailable(reason);
        Self {
            capabilities,
            profile: CompatibilityProfile {
                waldo_version: env!("CARGO_PKG_VERSION").to_owned(),
                niri_version: None,
                wayland_display: env::var("WAYLAND_DISPLAY").ok(),
                protocols: Vec::new(),
            },
            outputs: Vec::new(),
            workspaces: Vec::new(),
            windows: Vec::new(),
            focused_window: None,
            keyboard_layouts: Vec::new(),
            active_keyboard_layout: None,
        }
    }
}

/// The compositor's active keyboard layout: its group index into the
/// keymap and its name, as Niri reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveKeyboardLayout {
    pub index: u32,
    pub name: Option<String>,
}

/// Reads just the keyboard layouts from Niri, cheaply enough to run before
/// every keyboard action.
pub fn active_keyboard_layout() -> Result<ActiveKeyboardLayout, InspectionError> {
    let mut socket = Socket::connect()?;
    let keyboard = response_keyboard(send(&mut socket, Request::KeyboardLayouts)?)?;
    let index = u32::from(keyboard.current_idx);
    Ok(ActiveKeyboardLayout {
        name: keyboard.names.get(index as usize).cloned(),
        index,
    })
}

/// Read-only desktop discovery seam used by the controller service.
pub trait SessionInspector: Send + Sync {
    fn inspect(&self, include_windows: bool) -> Result<NiriSnapshot, InspectionError>;
}

/// Direct inspector for the Niri IPC socket and Wayland registry.
#[derive(Clone, Copy, Debug, Default)]
pub struct NiriInspector;

impl NiriInspector {
    pub fn inspect(&self, include_windows: bool) -> Result<NiriSnapshot, InspectionError> {
        let protocols = wayland_registry::advertised_protocols()?;
        let mut socket = Socket::connect()?;
        let version = response_version(send(&mut socket, Request::Version)?)?;
        let raw_outputs = response_outputs(send(&mut socket, Request::Outputs)?)?;
        let raw_workspaces = response_workspaces(send(&mut socket, Request::Workspaces)?)?;
        let focused_output = response_focused_output(send(&mut socket, Request::FocusedOutput)?)?;
        let focused_window = response_focused_window(send(&mut socket, Request::FocusedWindow)?)?;
        let keyboard = response_keyboard(send(&mut socket, Request::KeyboardLayouts)?)?;
        let raw_windows = if include_windows {
            response_windows(send(&mut socket, Request::Windows)?)?
        } else {
            Vec::new()
        };

        let focused_output_name = focused_output.as_ref().map(|output| output.name.as_str());
        let outputs = convert::outputs(raw_outputs, focused_output_name);
        let workspaces = convert::workspaces(&raw_workspaces);
        let focused_window = focused_window.map(|window| WindowId(window.id.to_string()));
        let windows = convert::windows(raw_windows, &raw_workspaces, &outputs);
        let active_keyboard_layout = keyboard
            .names
            .get(usize::from(keyboard.current_idx))
            .cloned();

        Ok(NiriSnapshot {
            capabilities: discovered_capabilities(&version, &protocols),
            profile: CompatibilityProfile {
                waldo_version: env!("CARGO_PKG_VERSION").to_owned(),
                niri_version: Some(version),
                wayland_display: env::var("WAYLAND_DISPLAY").ok(),
                protocols,
            },
            outputs,
            workspaces,
            windows,
            focused_window,
            keyboard_layouts: keyboard.names,
            active_keyboard_layout,
        })
    }
}

impl SessionInspector for NiriInspector {
    fn inspect(&self, include_windows: bool) -> Result<NiriSnapshot, InspectionError> {
        Self::inspect(self, include_windows)
    }
}

/// Deterministic inspector for transport tests without a desktop session.
#[derive(Clone, Debug)]
pub struct UnavailableInspector {
    reason: String,
}

impl UnavailableInspector {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl SessionInspector for UnavailableInspector {
    fn inspect(&self, _include_windows: bool) -> Result<NiriSnapshot, InspectionError> {
        Ok(NiriSnapshot::unavailable(self.reason.clone()))
    }
}

fn send(socket: &mut Socket, request: Request) -> Result<Response, InspectionError> {
    socket.send(request)?.map_err(InspectionError::NiriReply)
}

fn response_version(response: Response) -> Result<String, InspectionError> {
    match response {
        Response::Version(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("version")),
    }
}

fn response_outputs(response: Response) -> Result<HashMap<String, Output>, InspectionError> {
    match response {
        Response::Outputs(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("outputs")),
    }
}

fn response_workspaces(response: Response) -> Result<Vec<Workspace>, InspectionError> {
    match response {
        Response::Workspaces(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("workspaces")),
    }
}

fn response_windows(response: Response) -> Result<Vec<Window>, InspectionError> {
    match response {
        Response::Windows(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("windows")),
    }
}

fn response_focused_output(response: Response) -> Result<Option<Output>, InspectionError> {
    match response {
        Response::FocusedOutput(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("focused output")),
    }
}

fn response_focused_window(response: Response) -> Result<Option<Window>, InspectionError> {
    match response {
        Response::FocusedWindow(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("focused window")),
    }
}

fn response_keyboard(response: Response) -> Result<KeyboardLayouts, InspectionError> {
    match response {
        Response::KeyboardLayouts(value) => Ok(value),
        _ => Err(InspectionError::UnexpectedNiriReply("keyboard layouts")),
    }
}

fn discovered_capabilities(version: &str, protocols: &[ProtocolAdvertisement]) -> Capabilities {
    let advertised = |interface: &str| {
        protocols
            .iter()
            .find(|protocol| protocol.interface == interface)
            .map(|protocol| protocol.version.to_string())
    };
    let pending = |interface: &str, backend: &str| match advertised(interface) {
        Some(version) => BackendCapability {
            available: false,
            version: Some(version),
            limitation: Some(format!("protocol advertised; {backend} is not initialised")),
        },
        None => BackendCapability::unavailable(format!("{interface} is not advertised")),
    };

    Capabilities {
        screencopy: pending("zwlr_screencopy_manager_v1", "capture backend"),
        virtual_pointer: pending("zwlr_virtual_pointer_manager_v1", "pointer backend"),
        virtual_keyboard: pending("zwp_virtual_keyboard_manager_v1", "keyboard backend"),
        niri_ipc: BackendCapability {
            available: true,
            version: Some(version.to_owned()),
            limitation: None,
        },
        accessibility: BackendCapability::unavailable("AT-SPI has not been probed"),
        clipboard: pending("zwlr_data_control_manager_v1", "clipboard backend"),
        physical_input_detection: BackendCapability::unavailable(
            "physical device distinction has not been verified",
        ),
        driving_indicator: BackendCapability::unavailable(
            "no driving-indicator backend is implemented",
        ),
    }
}

#[cfg(test)]
mod capability_tests {
    use super::{ProtocolAdvertisement, discovered_capabilities};

    #[test]
    fn layer_shell_advertisement_does_not_imply_an_indicator_backend() {
        for protocols in [
            vec![],
            vec![ProtocolAdvertisement {
                interface: "zwlr_layer_shell_v1".into(),
                version: 1,
            }],
        ] {
            let indicator = discovered_capabilities("test", &protocols).driving_indicator;
            assert!(!indicator.available);
            assert_eq!(indicator.version, None);
            assert_eq!(
                indicator.limitation.as_deref(),
                Some("no driving-indicator backend is implemented")
            );
        }
    }
}
