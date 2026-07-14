use std::{error::Error, fmt};

use waldo_socket::TransportError;

#[derive(Debug)]
pub enum McpError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Controller(TransportError),
    InvalidClientId,
}

impl fmt::Display for McpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "MCP stdio failed: {error}"),
            Self::Json(error) => write!(formatter, "MCP message is invalid JSON: {error}"),
            Self::Controller(error) => write!(formatter, "Waldo controller failed: {error}"),
            Self::InvalidClientId => formatter.write_str("the MCP client identity is invalid"),
        }
    }
}

impl Error for McpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Controller(error) => Some(error),
            Self::InvalidClientId => None,
        }
    }
}

impl From<std::io::Error> for McpError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for McpError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<TransportError> for McpError {
    fn from(value: TransportError) -> Self {
        Self::Controller(value)
    }
}
