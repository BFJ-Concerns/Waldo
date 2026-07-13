//! Niri session discovery and positively targeted compositor actions.

mod action;
mod convert;
mod error;
mod inspect;
mod wayland_registry;

pub use action::NiriActionExecutor;
pub use error::InspectionError;
pub use inspect::{NiriInspector, NiriSnapshot, SessionInspector, UnavailableInspector};
