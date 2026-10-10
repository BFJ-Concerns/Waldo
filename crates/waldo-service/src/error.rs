use std::{error::Error, fmt, io, path::PathBuf};

use waldo_socket::TransportError;

#[derive(Debug)]
pub enum ServiceError {
    Io(io::Error),
    Settings(waldo_settings::SettingsError),
    Transport(TransportError),
    UnsafeSocketPath(String),
    ControllerAlreadyServing(PathBuf),
    PeerCredential(String),
    ConnectionTask(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "controller service I/O failed: {error}"),
            Self::Transport(error) => error.fmt(formatter),
            Self::Settings(error) => error.fmt(formatter),
            Self::UnsafeSocketPath(reason) => {
                write!(
                    formatter,
                    "refusing unsafe controller socket path: {reason}"
                )
            }
            Self::ControllerAlreadyServing(path) => write!(
                formatter,
                "another controller is already serving {}",
                path.display()
            ),
            Self::PeerCredential(reason) => {
                write!(formatter, "controller peer authentication failed: {reason}")
            }
            Self::ConnectionTask(reason) => {
                write!(formatter, "controller request task failed: {reason}")
            }
        }
    }
}

impl Error for ServiceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Settings(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ServiceError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<TransportError> for ServiceError {
    fn from(value: TransportError) -> Self {
        Self::Transport(value)
    }
}

impl From<waldo_settings::SettingsError> for ServiceError {
    fn from(value: waldo_settings::SettingsError) -> Self {
        Self::Settings(value)
    }
}
