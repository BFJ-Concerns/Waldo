use std::{error::Error, fmt, io};

#[derive(Debug)]
pub enum InspectionError {
    NiriIo(io::Error),
    NiriReply(String),
    UnexpectedNiriReply(&'static str),
    WaylandConnect(String),
    WaylandRegistry(String),
}

impl fmt::Display for InspectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NiriIo(error) => write!(formatter, "Niri IPC failed: {error}"),
            Self::NiriReply(error) => write!(formatter, "Niri refused the query: {error}"),
            Self::UnexpectedNiriReply(expected) => {
                write!(
                    formatter,
                    "Niri returned an unexpected response; expected {expected}"
                )
            }
            Self::WaylandConnect(error) => {
                write!(
                    formatter,
                    "could not connect to the Wayland session: {error}"
                )
            }
            Self::WaylandRegistry(error) => {
                write!(formatter, "could not enumerate Wayland protocols: {error}")
            }
        }
    }
}

impl Error for InspectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NiriIo(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for InspectionError {
    fn from(value: io::Error) -> Self {
        Self::NiriIo(value)
    }
}
