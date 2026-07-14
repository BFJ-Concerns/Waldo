//! Waldo's versioned, transport-independent wire contract.
//!
//! The CLI, controller socket, and MCP binding all serialise these exact types.
//! Desktop backends implement them; they do not define transport-specific
//! alternatives.

mod action;
mod capability;
mod error;
mod geometry;
mod identifiers;
mod message;
mod observation;
mod schema;
mod selector;

pub use action::*;
pub use capability::*;
pub use error::*;
pub use geometry::*;
pub use identifiers::*;
pub use message::*;
pub use observation::*;
pub use schema::*;
pub use selector::*;

/// The major version of Waldo's transport-independent protocol.
///
/// Bindings may add ignorable result fields within this version. Incompatible
/// request changes require a new major version.
pub const SCHEMA_MAJOR_VERSION: u16 = 2;
