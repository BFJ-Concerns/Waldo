use std::{error::Error, fmt, io};

use waldo_socket::TransportError;

#[derive(Debug)]
pub enum ServiceError {
    Io(io::Error),
    Transport(TransportError),
    UnsafeSocketPath(String),
    PeerCredential(String),
    ConnectionTask(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "controller service I/O failed: {error}"),
            Self::Transport(error) => error.fmt(formatter),
            Self::UnsafeSocketPath(reason) => {
                write!(
                    formatter,
                    "refusing unsafe controller socket path: {reason}"
                )
            }
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
