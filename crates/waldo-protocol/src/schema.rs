use schemars::{Schema, schema_for};
use serde::Serialize;

use crate::{ClientMessage, SCHEMA_MAJOR_VERSION, ServerMessage};

/// The single published schema document consumed by every Waldo binding.
#[derive(Debug, Serialize)]
pub struct PublishedSchema {
    pub schema_major: u16,
    pub client_messages: Schema,
    pub server_messages: Schema,
}

/// Generates the current machine-readable JSON schemas from the wire types.
pub fn published_schema() -> PublishedSchema {
    PublishedSchema {
        schema_major: SCHEMA_MAJOR_VERSION,
        client_messages: schema_for!(ClientMessage),
        server_messages: schema_for!(ServerMessage),
    }
}

#[cfg(test)]
mod tests {
    use crate::SCHEMA_MAJOR_VERSION;

    #[test]
    fn current_schema_uses_major_version_two() {
        assert_eq!(SCHEMA_MAJOR_VERSION, 2);
    }
}
