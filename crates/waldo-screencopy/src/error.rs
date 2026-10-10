use std::{error::Error, fmt, io};

#[derive(Debug)]
pub enum CaptureError {
    Wayland(String),
    ProtocolMissing(&'static str),
    OutputMissing(String),
    UnsupportedFormat(String),
    InvalidBuffer(String),
    TimedOut,
    CompositorFailed,
    Io(io::Error),
    Image(image::ImageError),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wayland(error) => write!(formatter, "Wayland screencopy failed: {error}"),
            Self::ProtocolMissing(protocol) => {
                write!(
                    formatter,
                    "required Wayland protocol is missing: {protocol}"
                )
            }
            Self::OutputMissing(output) => {
                write!(formatter, "Niri output is unavailable: {output}")
            }
            Self::UnsupportedFormat(format) => {
                write!(formatter, "unsupported screencopy pixel format: {format}")
            }
            Self::InvalidBuffer(reason) => write!(formatter, "invalid screencopy buffer: {reason}"),
            Self::TimedOut => formatter.write_str("Niri screencopy timed out"),
            Self::CompositorFailed => formatter.write_str("Niri reported screencopy failure"),
            Self::Io(error) => write!(formatter, "screencopy I/O failed: {error}"),
            Self::Image(error) => write!(formatter, "PNG encoding failed: {error}"),
        }
    }
}

impl Error for CaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Image(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for CaptureError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<image::ImageError> for CaptureError {
    fn from(value: image::ImageError) -> Self {
        Self::Image(value)
    }
}
