use std::fmt;

use niri_ipc::{Request, Response, Transform, socket::Socket};
use serde::{Deserialize, Serialize};

/// A serial-free Niri topology snapshot captured independently of Waldo.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologySnapshot {
    pub outputs: Vec<TopologyOutput>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyOutput {
    pub name: String,
    pub pixel_size: Option<TopologyPixelSize>,
    pub logical: Option<TopologyRect>,
    pub vrr_enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyPixelSize {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: TopologyTransform,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyTransform {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopologyError(String);

impl fmt::Display for TopologyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for TopologyError {}

pub fn capture_niri_topology() -> Result<TopologySnapshot, TopologyError> {
    let mut socket = Socket::connect().map_err(|error| TopologyError(error.to_string()))?;
    let response = socket
        .send(Request::Outputs)
        .map_err(|error| TopologyError(error.to_string()))?
        .map_err(TopologyError)?;
    let Response::Outputs(outputs) = response else {
        return Err(TopologyError(
            "Niri returned an unexpected response to Outputs".to_owned(),
        ));
    };

    let mut outputs = outputs
        .into_values()
        .map(|output| {
            let pixel_size = output
                .current_mode
                .and_then(|index| output.modes.get(index))
                .map(|mode| TopologyPixelSize {
                    width: mode.width,
                    height: mode.height,
                });
            let logical = output.logical.map(|logical| TopologyRect {
                x: logical.x,
                y: logical.y,
                width: logical.width,
                height: logical.height,
                scale: logical.scale,
                transform: convert_transform(logical.transform),
            });
            TopologyOutput {
                name: output.name,
                pixel_size,
                logical,
                vrr_enabled: output.vrr_enabled,
            }
        })
        .collect::<Vec<_>>();
    outputs.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(TopologySnapshot { outputs })
}

fn convert_transform(transform: Transform) -> TopologyTransform {
    match transform {
        Transform::Normal => TopologyTransform::Normal,
        Transform::_90 => TopologyTransform::Rotate90,
        Transform::_180 => TopologyTransform::Rotate180,
        Transform::_270 => TopologyTransform::Rotate270,
        Transform::Flipped => TopologyTransform::Flipped,
        Transform::Flipped90 => TopologyTransform::Flipped90,
        Transform::Flipped180 => TopologyTransform::Flipped180,
        Transform::Flipped270 => TopologyTransform::Flipped270,
    }
}
