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
    ActRequest, ClientId, ClientMessage, ConnectionId, ErrorCode, HandoverRequest, ObserveRequest,
    ProtocolError, QueryRequest, RequestId, ServerMessage, StopRequest, published_schema,
};
use waldo_socket::{PipelinedControllerClient, TransportError, default_socket_path};

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
        /// ObserveRequest document used to ground this action on the same connection.
        #[arg(long)]
        observe: Option<PathBuf>,
        /// Cancel this batch through Stop after the given delay.
        #[arg(long, conflicts_with = "handover_to")]
        stop_after_ms: Option<u64>,
        /// Transfer the acquired driving lease after this batch completes.
        #[arg(long, conflicts_with = "stop_after_ms")]
        handover_to: Option<u64>,
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
    SharedStdin,
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
            Self::SharedStdin => formatter.write_str(
                "the action and grounding observation documents cannot both use standard input",
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
            Self::HelloDocument | Self::SharedStdin => None,
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
    let client = PipelinedControllerClient::connect(&socket, client_id).await?;
    let request_id = RequestId::new(2);
    if let Command::Act {
        document,
        observe,
        stop_after_ms,
        handover_to,
    } = arguments.command
    {
        let mut request: ActRequest = match request_document(&document, RequestId::new(3))? {
            Ok(request) => request,
            Err(response) => return Ok(response),
        };
        if let Some(observe_document) = observe {
            if document == Path::new("-") && observe_document == Path::new("-") {
                return Err(CliError::SharedStdin);
            }
            let observation_request: ObserveRequest =
                match request_document(&observe_document, request_id)? {
                    Ok(request) => request,
                    Err(response) => return Ok(response),
                };
            let observed = client
                .request(ClientMessage::Observe {
                    request_id,
                    request: observation_request,
                })
                .await?;
            let ServerMessage::Observed { observation, .. } = observed else {
                return Ok(serde_json::to_value(observed)?);
            };
            request.observation_id = observation.id;
        }
        let act_message = ClientMessage::Act {
            request_id: RequestId::new(3),
            request,
        };
        if let Some(delay_ms) = stop_after_ms {
            let act_client = client.clone();
            let stop_client = client.clone();
            let act = async move { act_client.request(act_message).await };
            let stop = async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                stop_client
                    .request(ClientMessage::Stop {
                        request_id: RequestId::new(4),
                        request: StopRequest::default(),
                    })
                    .await
            };
            let (acted, stopped) = tokio::join!(act, stop);
            return json_envelope("act_cancelled", acted?, "stop", stopped?);
        }

        let acted = client.request(act_message).await?;
        if let Some(connection) = handover_to {
            let handed_over = client
                .request(ClientMessage::Handover {
                    request_id: RequestId::new(4),
                    request: HandoverRequest {
                        target: ConnectionId::new(connection),
                    },
                })
                .await?;
            return json_envelope("act_handover", acted, "handover", handed_over);
        }
        return Ok(serde_json::to_value(acted)?);
    }
    let message = match arguments.command {
        Command::Schema => unreachable!("schema returned before opening the controller socket"),
        Command::Query { include_windows } => ClientMessage::Query {
            request_id,
            request: QueryRequest { include_windows },
        },
        Command::Observe { document } => ClientMessage::Observe {
            request_id,
            request: match request_document(&document, request_id)? {
                Ok(request) => request,
                Err(response) => return Ok(response),
            },
        },
        Command::Act { .. } => unreachable!("Act returned through its composed CLI path"),
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
            let message: ClientMessage = match request_document(&document, request_id)? {
                Ok(request) => request,
                Err(response) => return Ok(response),
            };
            if matches!(message, ClientMessage::Hello { .. }) {
                return Err(CliError::HelloDocument);
            }
            message
        }
    };

    Ok(serde_json::to_value(client.request(message).await?)?)
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

fn request_document<T: DeserializeOwned>(
    path: &Path,
    request_id: RequestId,
) -> Result<Result<T, Value>, CliError> {
    match read_document(path) {
        Ok(request) => Ok(Ok(request)),
        Err(CliError::Json(_)) => Ok(Err(serde_json::to_value(ServerMessage::Error {
            request_id: Some(request_id),
            error: ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: "request document does not match the protocol schema".into(),
                retryable: false,
            },
        })?)),
        Err(error) => Err(error),
    }
}

fn json_envelope(
    kind: &str,
    acted: ServerMessage,
    following_name: &str,
    following: ServerMessage,
) -> Result<Value, CliError> {
    Ok(serde_json::json!({
        "type": kind,
        "act": acted,
        (following_name): following,
    }))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Arguments, Command};

    #[test]
    fn action_documents_have_a_direct_cli_route() {
        let arguments = Arguments::try_parse_from(["waldo", "act", "batch.json"]).unwrap();

        assert!(matches!(
            arguments.command,
            Command::Act { observe: None, .. }
        ));
    }

    #[test]
    fn act_can_ground_a_saved_batch_on_the_same_controller_connection() {
        let arguments =
            Arguments::try_parse_from(["waldo", "act", "batch.json", "--observe", "source.json"])
                .unwrap();

        assert!(matches!(
            arguments.command,
            Command::Act {
                observe: Some(path),
                ..
            } if path == std::path::Path::new("source.json")
        ));
    }

    #[test]
    fn act_can_compose_stop_or_handover_without_changing_the_saved_document() {
        let cancelled =
            Arguments::try_parse_from(["waldo", "act", "batch.json", "--stop-after-ms", "25"])
                .unwrap();
        let handed_over =
            Arguments::try_parse_from(["waldo", "act", "batch.json", "--handover-to", "7"])
                .unwrap();

        assert!(matches!(
            cancelled.command,
            Command::Act {
                stop_after_ms: Some(25),
                handover_to: None,
                ..
            }
        ));
        assert!(matches!(
            handed_over.command,
            Command::Act {
                stop_after_ms: None,
                handover_to: Some(7),
                ..
            }
        ));
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
