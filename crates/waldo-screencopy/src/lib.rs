//! In-memory capture through Niri's `zwlr_screencopy_manager_v1` global.

mod capture;
mod encode;
mod error;
mod shm;

pub use capture::{Capture, Screencopy};
pub use error::CaptureError;

/// Read-only capture seam used by the controller service.
pub trait ScreenCapturer: Send + Sync {
    fn available(&self) -> bool;
    fn capture_output(&self, output_name: &str) -> Result<Capture, CaptureError>;
    fn capture_region(
        &self,
        output_name: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<Capture, CaptureError>;
}

impl ScreenCapturer for Screencopy {
    fn available(&self) -> bool {
        true
    }

    fn capture_output(&self, output_name: &str) -> Result<Capture, CaptureError> {
        Self::capture_output(self, output_name)
    }

    fn capture_region(
        &self,
        output_name: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<Capture, CaptureError> {
        Self::capture_region(self, output_name, x, y, width, height)
    }
}

/// Capture seam for controller transport tests without a live compositor.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableCapturer;

impl ScreenCapturer for UnavailableCapturer {
    fn available(&self) -> bool {
        false
    }

    fn capture_output(&self, _output_name: &str) -> Result<Capture, CaptureError> {
        Err(CaptureError::ProtocolMissing("zwlr_screencopy_manager_v1"))
    }

    fn capture_region(
        &self,
        _output_name: &str,
        _x: i32,
        _y: i32,
        _width: i32,
        _height: i32,
    ) -> Result<Capture, CaptureError> {
        Err(CaptureError::ProtocolMissing("zwlr_screencopy_manager_v1"))
    }
}
