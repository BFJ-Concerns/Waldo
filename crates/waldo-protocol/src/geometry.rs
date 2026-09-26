use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A position in the compositor's logical desktop coordinate space.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalPoint {
    pub x: f64,
    pub y: f64,
}

/// A relative movement in logical desktop coordinates.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalDelta {
    pub x: f64,
    pub y: f64,
}

/// A rectangle in the compositor's logical desktop coordinate space.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalRect {
    pub origin: LogicalPoint,
    pub width: f64,
    pub height: f64,
}

/// Integer pixel dimensions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

/// The transform applied by the compositor to an output buffer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTransform {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

/// The mappings needed to ground model-image points in desktop input space.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct CoordinateMapping {
    /// Desktop-oriented capture dimensions before presentation resizing.
    pub source_pixels: PixelSize,
    /// Exact decoded dimensions of the image delivered to the model consumer.
    pub model_image_pixels: PixelSize,
    /// Desktop-logical rectangle represented by the complete delivered image.
    ///
    /// A model-image point maps linearly into this rectangle. For example,
    /// `logical_x = origin.x + image_x * width / model_image_pixels.width`.
    pub logical_bounds: LogicalRect,
    pub scale: f64,
    /// Output buffer transform, already accounted for in the delivered image.
    pub transform: OutputTransform,
}
