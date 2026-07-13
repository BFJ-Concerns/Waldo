//! The single private-socket authority for Waldo controller clients.

mod act;
mod dispatch;
mod error;
mod observe;
mod server;
mod socket_path;

pub use error::ServiceError;
pub use server::ControllerService;
pub use waldo_socket::default_socket_path;
