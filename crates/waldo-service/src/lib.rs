//! The single private-socket authority for Waldo controller clients.

mod act;
mod batch;
mod dispatch;
mod error;
mod executor;
mod observe;
mod server;
mod socket_path;
mod verification;

pub use error::ServiceError;
pub use server::ControllerService;
