//! Transport-independent request and result types for Waldo's public bindings.

use std::{error::Error, fmt};

/// The major version of Waldo's transport-independent protocol.
///
/// Bindings may add ignorable result fields within this version. Incompatible
/// request changes require a new major version.
pub const SCHEMA_MAJOR_VERSION: u16 = 1;

/// Stable identity supplied by a logical controller client.
///
/// A client may reconnect with the same identity, but each connection receives
/// a new [`ConnectionId`]. This distinction prevents a dropped connection from
/// silently recovering its former driving authority.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId(String);

impl ClientId {
    /// Creates a client identity after rejecting an empty or whitespace-only
    /// value.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidClientId> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(InvalidClientId);
        }

        Ok(Self(value))
    }

    /// Returns the client-supplied identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A client identity was empty or contained only whitespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidClientId;

impl fmt::Display for InvalidClientId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a client identifier cannot be empty")
    }
}

impl Error for InvalidClientId {}

/// Session-local identity for one controller connection.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// Constructs an identifier from the controller's monotonic sequence.
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Returns the sequence value used by machine-readable bindings.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Session-local identity for one period of driving authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DrivingSessionId(u64);

impl DrivingSessionId {
    /// Constructs an identifier from the controller's monotonic sequence.
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Returns the sequence value used by machine-readable bindings.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientId, SCHEMA_MAJOR_VERSION};

    #[test]
    fn client_identifiers_must_contain_visible_content() {
        assert!(ClientId::new("").is_err());
        assert!(ClientId::new("  \n").is_err());
        assert_eq!(ClientId::new("mcp").unwrap().as_str(), "mcp");
    }

    #[test]
    fn initial_schema_uses_major_version_one() {
        assert_eq!(SCHEMA_MAJOR_VERSION, 1);
    }
}
