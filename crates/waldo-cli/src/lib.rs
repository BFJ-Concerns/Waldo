//! Full-fidelity command-line binding for Waldo's controller protocol.

use std::{
    error::Error,
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use clap::{Parser, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::Value;
use waldo_protocol::{
    ClientId, ClientMessage, ConnectionId, HandoverRequest, QueryRequest, RequestId, StopRequest,
    published_schema,
};
use waldo_socket::{ControllerClient, TransportError, default_socket_path};

#[derive(Debug, Parser)]
#[command(name = "waldo", about = "Observe and control the active Niri desktop")]
pub struct Arguments {
    /// Override the private controller socket path.
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Logical identity shown in controller diagnostics.
    #[arg(long, global = true, default_value = "waldo-cli")]
    pub client_id: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the complete versioned protocol schema without contacting a controller.
    Schema,
    /// Inspect capabilities and current desktop/controller state.
    Query {
        #[arg(long)]
        include_windows: bool,
    },
    /// Capture a source described by an ObserveRequest JSON document.
    Observe {
        /// JSON document path, or '-' for standard input.
        document: PathBuf,
    },
    /// Execute an ActRequest JSON document.
    Act {
        /// JSON document path, or '-' for standard input.
        document: PathBuf,
    },
    /// Cancel and release this connection's driving session.
    Stop,
    /// Transfer this connection's driving lease to another connection.
    Handover {
        #[arg(long)]
        connection: u64,
    },
    /// Send any non-Hello ClientMessage document without reshaping it.
    Send {
        /// JSON document path, or '-' for standard input.
        document: PathBuf,
    },
}

#[derive(Debug)]
pub enum CliError {
    Io(io::Error),
    Json(serde_json::Error),
    Transport(TransportError),
    InvalidClientId(waldo_protocol::InvalidClientId),
    HelloDocument,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not read action document: {error}"),
            Self::Json(error) => write!(formatter, "action document is invalid: {error}"),
            Self::Transport(error) => error.fmt(formatter),
            Self::InvalidClientId(error) => error.fmt(formatter),
            Self::HelloDocument => formatter.write_str(
                "send documents cannot contain hello; the CLI performs the authenticated handshake",
            ),
        }
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::InvalidClientId(error) => Some(error),
            Self::HelloDocument => None,
        }
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<TransportError> for CliError {
    fn from(value: TransportError) -> Self {
        Self::Transport(value)
    }
}

/// Executes one CLI invocation and returns its JSON value for rendering.
pub async fn execute(arguments: Arguments) -> Result<Value, CliError> {
    if matches!(arguments.command, Command::Schema) {
        return Ok(serde_json::to_value(published_schema())?);
    }

    let socket = match arguments.socket {
        Some(path) => path,
        None => default_socket_path()?,
    };
    let client_id = ClientId::new(arguments.client_id).map_err(CliError::InvalidClientId)?;
    let mut client = ControllerClient::connect(&socket, client_id).await?;
    let request_id = RequestId::new(2);
    let message = match arguments.command {
        Command::Schema => unreachable!("schema returned before opening the controller socket"),
        Command::Query { include_windows } => ClientMessage::Query {
            request_id,
            request: QueryRequest { include_windows },
        },
        Command::Observe { document } => ClientMessage::Observe {
            request_id,
            request: read_document(&document)?,
        },
        Command::Act { document } => ClientMessage::Act {
            request_id,
            request: read_document(&document)?,
        },
        Command::Stop => ClientMessage::Stop {
            request_id,
            request: StopRequest::default(),
        },
        Command::Handover { connection } => ClientMessage::Handover {
            request_id,
            request: HandoverRequest {
                target: ConnectionId::new(connection),
            },
        },
        Command::Send { document } => {
            let message: ClientMessage = read_document(&document)?;
            if matches!(message, ClientMessage::Hello { .. }) {
                return Err(CliError::HelloDocument);
            }
            message
        }
    };

    Ok(serde_json::to_value(client.request(&message).await?)?)
}

fn read_document<T: DeserializeOwned>(path: &Path) -> Result<T, CliError> {
    let mut document = String::new();
    if path == Path::new("-") {
        io::stdin().read_to_string(&mut document)?;
    } else {
        document = fs::read_to_string(path)?;
    }
    Ok(serde_json::from_str(&document)?)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Arguments, Command};

    #[test]
    fn action_documents_have_a_direct_cli_route() {
        let arguments = Arguments::try_parse_from(["waldo", "act", "batch.json"]).unwrap();

        assert!(matches!(arguments.command, Command::Act { .. }));
    }

    #[test]
    fn socket_override_is_global() {
        let arguments =
            Arguments::try_parse_from(["waldo", "query", "--socket", "/tmp/waldo.sock"]).unwrap();

        assert_eq!(
            arguments.socket.unwrap(),
            std::path::Path::new("/tmp/waldo.sock")
        );
    }
}
