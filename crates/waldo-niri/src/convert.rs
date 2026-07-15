use std::collections::HashMap;

use niri_ipc::{Output, Transform, Window, Workspace};
use waldo_protocol::{
    LogicalPoint, LogicalRect, OutputId, OutputState, OutputTransform, PixelSize,
    WindowCaptureCapability, WindowId, WindowState, WorkspaceId, WorkspaceState,
};

pub(crate) fn outputs(
    source: HashMap<String, Output>,
    focused_name: Option<&str>,
) -> Vec<OutputState> {
    let mut outputs = source
        .into_values()
        .filter_map(|output| {
            let logical = output.logical?;
            let pixel_size = output
                .current_mode
                .and_then(|index| output.modes.get(index))
                .map(|mode| PixelSize {
                    width: u32::from(mode.width),
                    height: u32::from(mode.height),
                });
            Some(OutputState {
                id: OutputId(output.name.clone()),
                name: output.name.clone(),
                logical_bounds: LogicalRect {
                    origin: LogicalPoint {
                        x: f64::from(logical.x),
                        y: f64::from(logical.y),
                    },
                    width: f64::from(logical.width),
                    height: f64::from(logical.height),
                },
                pixel_size,
                scale: logical.scale,
                transform: output_transform(logical.transform),
                focused: focused_name == Some(output.name.as_str()),
            })
        })
        .collect::<Vec<_>>();
    outputs.sort_by(|left, right| left.name.cmp(&right.name));
    outputs
}

fn output_transform(transform: Transform) -> OutputTransform {
    match transform {
        Transform::Normal => OutputTransform::Normal,
        Transform::_90 => OutputTransform::Rotate90,
        Transform::_180 => OutputTransform::Rotate180,
        Transform::_270 => OutputTransform::Rotate270,
        Transform::Flipped => OutputTransform::Flipped,
        Transform::Flipped90 => OutputTransform::Flipped90,
        Transform::Flipped180 => OutputTransform::Flipped180,
        Transform::Flipped270 => OutputTransform::Flipped270,
    }
}

pub(crate) fn workspaces(source: &[Workspace]) -> Vec<WorkspaceState> {
    source
        .iter()
        .map(|workspace| WorkspaceState {
            id: WorkspaceId::new(workspace.id),
            index: workspace.idx,
            name: workspace.name.clone(),
            output_id: workspace.output.clone().map(OutputId),
            active: workspace.is_active,
            focused: workspace.is_focused,
        })
        .collect()
}

pub(crate) fn windows(
    source: Vec<Window>,
    workspaces: &[Workspace],
    outputs: &[OutputState],
) -> Vec<WindowState> {
    let workspace_outputs = workspaces
        .iter()
        .filter_map(|workspace| Some((workspace.id, workspace.output.as_ref()?)))
        .collect::<HashMap<_, _>>();
    let output_bounds = outputs
        .iter()
        .map(|output| (output.name.as_str(), output.logical_bounds))
        .collect::<HashMap<_, _>>();

    source
        .into_iter()
        .map(|window| {
            let output_name = window
                .workspace_id
                .and_then(|workspace| workspace_outputs.get(&workspace).copied());
            let logical_bounds = output_name
                .and_then(|name| output_bounds.get(name.as_str()).copied())
                .and_then(|output| window_bounds(&window, output));
            let capture = window_capture_capability(window.is_floating, logical_bounds.is_some());
            WindowState {
                id: WindowId(window.id.to_string()),
                title: window.title,
                application_id: window.app_id,
                pid: window.pid.and_then(|pid| u32::try_from(pid).ok()),
                logical_bounds,
                is_floating: window.is_floating,
                capture,
                focused: window.is_focused,
                workspace_id: window.workspace_id.map(WorkspaceId::new),
                output_id: output_name.cloned().map(OutputId),
            }
        })
        .collect()
}

fn window_capture_capability(
    is_floating: bool,
    has_logical_bounds: bool,
) -> WindowCaptureCapability {
    if !is_floating {
        return WindowCaptureCapability {
            available: false,
            limitation: Some(
                "tiled-window capture is unavailable; observe the containing output instead".into(),
            ),
        };
    }
    if !has_logical_bounds {
        return WindowCaptureCapability {
            available: false,
            limitation: Some("Niri has not reported screen bounds for this floating window".into()),
        };
    }
    WindowCaptureCapability {
        available: true,
        limitation: Some(
            "capture is an exact output crop and requires the window to be foreground and visible"
                .into(),
        ),
    }
}

fn window_bounds(window: &Window, output: LogicalRect) -> Option<LogicalRect> {
    let (tile_x, tile_y) = window.layout.tile_pos_in_workspace_view?;
    let (offset_x, offset_y) = window.layout.window_offset_in_tile;
    let (width, height) = window.layout.window_size;
    Some(LogicalRect {
        origin: LogicalPoint {
            x: output.origin.x + tile_x + offset_x,
            y: output.origin.y + tile_y + offset_y,
        },
        width: f64::from(width),
        height: f64::from(height),
    })
}

#[cfg(test)]
mod tests {
    use niri_ipc::{LogicalOutput, Mode, Output, Transform, Window, WindowLayout, Workspace};

    use super::{outputs, windows};

    fn output(name: &str, x: i32) -> Output {
        Output {
            name: name.into(),
            make: "Test".into(),
            model: "Panel".into(),
            serial: None,
            physical_size: None,
            modes: vec![Mode {
                width: 1920,
                height: 1080,
                refresh_rate: 60_000,
                is_preferred: true,
            }],
            current_mode: Some(0),
            is_custom_mode: false,
            vrr_supported: false,
            vrr_enabled: false,
            logical: Some(LogicalOutput {
                x,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                transform: Transform::Normal,
            }),
        }
    }

    fn workspace() -> Workspace {
        Workspace {
            id: 5,
            idx: 1,
            name: None,
            output: Some("DP-2".into()),
            is_urgent: false,
            is_active: true,
            is_focused: true,
            active_window_id: Some(9),
        }
    }

    fn window(position: Option<(f64, f64)>) -> Window {
        Window {
            id: 9,
            title: Some("Editor".into()),
            app_id: Some("editor".into()),
            pid: Some(42),
            workspace_id: Some(5),
            is_focused: true,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (800.0, 600.0),
                window_size: (790, 590),
                tile_pos_in_workspace_view: position,
                window_offset_in_tile: (5.0, 5.0),
            },
            focus_timestamp: None,
        }
    }

    #[test]
    fn output_and_window_geometry_share_the_logical_desktop_space() {
        let converted_outputs = outputs(
            [("DP-2".to_owned(), output("DP-2", 1920))]
                .into_iter()
                .collect(),
            Some("DP-2"),
        );

        let converted_windows = windows(
            vec![window(Some((20.0, 30.0)))],
            &[workspace()],
            &converted_outputs,
        );

        assert!(converted_outputs[0].focused);
        assert_eq!(converted_outputs[0].pixel_size.unwrap().width, 1920);
        let bounds = converted_windows[0].logical_bounds.unwrap();
        assert_eq!(bounds.origin.x, 1945.0);
        assert_eq!(bounds.origin.y, 35.0);
        assert_eq!(bounds.width, 790.0);
    }

    #[test]
    fn missing_window_position_stays_unknown_instead_of_being_guessed() {
        let converted_outputs = outputs(
            [("DP-2".to_owned(), output("DP-2", 0))]
                .into_iter()
                .collect(),
            None,
        );

        let converted_windows = windows(vec![window(None)], &[workspace()], &converted_outputs);

        assert_eq!(converted_windows[0].logical_bounds, None);
        assert!(!converted_windows[0].capture.available);
    }

    #[test]
    fn tiled_windows_disclose_that_direct_capture_is_unsupported() {
        let converted_outputs = outputs(
            [("DP-2".to_owned(), output("DP-2", 0))]
                .into_iter()
                .collect(),
            None,
        );

        let converted_windows = windows(
            vec![window(Some((20.0, 30.0)))],
            &[workspace()],
            &converted_outputs,
        );

        assert!(!converted_windows[0].is_floating);
        assert!(!converted_windows[0].capture.available);
        assert!(
            converted_windows[0]
                .capture
                .limitation
                .as_deref()
                .is_some_and(|limitation| limitation.contains("tiled-window"))
        );
    }

    #[test]
    fn bounded_floating_windows_disclose_the_output_crop_route() {
        let converted_outputs = outputs(
            [("DP-2".to_owned(), output("DP-2", 0))]
                .into_iter()
                .collect(),
            None,
        );
        let mut source = window(Some((20.0, 30.0)));
        source.is_floating = true;

        let converted_windows = windows(vec![source], &[workspace()], &converted_outputs);

        assert!(converted_windows[0].is_floating);
        assert!(converted_windows[0].capture.available);
        assert!(
            converted_windows[0]
                .capture
                .limitation
                .as_deref()
                .is_some_and(|limitation| limitation.contains("exact output crop"))
        );
    }
}
