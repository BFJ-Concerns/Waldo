use std::{error::Error, fmt};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

/// Reserved client-supplied value retained for wire compatibility.
///
/// The controller does not read this value; driving authority belongs to
/// the independently assigned [`ConnectionId`].
#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ClientId(
    /// At least one non-whitespace character, the rule [`ClientId::new`]
    /// enforces on every decoded value.
    #[schemars(regex(pattern = r"\S"))]
    String,
);

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
            /// Constructs an identifier from its wire value.
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the wire value used by machine-readable bindings.
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

numeric_identifier!(
    /// Identity for one request on a controller connection, chosen by the
    /// client that sends it.
    RequestId
);
numeric_identifier!(
    /// Session-local identity for one controller connection, assigned by the
    /// controller.
    ConnectionId
);
numeric_identifier!(
    /// Session-local identity for one period of driving authority, assigned
    /// by the controller.
    DrivingSessionId
);
numeric_identifier!(
    /// Session-local identity for one captured desktop observation, assigned
    /// by the controller.
    ObservationId
);
numeric_identifier!(
    /// Opaque identity for one driving-session clipboard preservation guard,
    /// assigned by the controller.
    ClipboardPreservationId
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
    fn the_published_client_identifier_schema_states_the_decoding_rule() {
        let schema = serde_json::to_value(schemars::schema_for!(ClientId)).unwrap();

        assert_eq!(schema["type"], "string");
        assert_eq!(schema["pattern"], r"\S");
    }

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
