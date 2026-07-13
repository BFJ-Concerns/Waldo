use std::{error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Stable identity supplied by a logical controller client.
///
/// A client may reconnect with the same identity, but each connection receives
/// a new [`ConnectionId`]. This distinction prevents a dropped connection from
/// silently recovering its former driving authority.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
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

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
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

macro_rules! numeric_identifier {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd,
            Serialize,
        )]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(u64);

        impl $name {
            /// Constructs an identifier from the controller's monotonic sequence.
            pub const fn new(sequence: u64) -> Self {
                Self(sequence)
            }

            /// Returns the sequence value used by machine-readable bindings.
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

numeric_identifier!(
    /// Identity for one request on a controller connection.
    RequestId
);
numeric_identifier!(
    /// Session-local identity for one controller connection.
    ConnectionId
);
numeric_identifier!(
    /// Session-local identity for one period of driving authority.
    DrivingSessionId
);
numeric_identifier!(
    /// Session-local identity for one captured desktop observation.
    ObservationId
);
numeric_identifier!(
    /// Stable Niri identity for one workspace during the compositor session.
    WorkspaceId
);

macro_rules! text_identifier {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        #[schemars(transparent)]
        pub struct $name(pub String);
    };
}

text_identifier!(
    /// Stable identifier for a compositor output within the active session.
    OutputId
);
text_identifier!(
    /// Stable identifier for a positively identified Niri window.
    WindowId
);
text_identifier!(
    /// Session-local opaque handle for an accessibility element.
    ElementHandle
);

#[cfg(test)]
mod tests {
    use super::ClientId;

    #[test]
    fn client_identifiers_must_contain_visible_content() {
        assert!(ClientId::new("").is_err());
        assert!(ClientId::new("  \n").is_err());
        assert_eq!(ClientId::new("mcp").unwrap().as_str(), "mcp");
    }

    #[test]
    fn deserialisation_applies_client_identifier_validation() {
        assert!(serde_json::from_str::<ClientId>(r#"" ""#).is_err());
    }
}
