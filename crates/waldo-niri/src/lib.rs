//! Read-only discovery for the active Niri Wayland session.

mod convert;
mod error;
mod inspect;
mod wayland_registry;

pub use error::InspectionError;
pub use inspect::{NiriInspector, NiriSnapshot, SessionInspector, UnavailableInspector};
