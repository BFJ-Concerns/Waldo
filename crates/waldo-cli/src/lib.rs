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
    ProtocolError, QueryRequest, RequestId, ServerMessage, StopRequest, decode_error_detail,
    published_schema,
};
use waldo_socket::{PipelinedControllerClient, TransportError, default_socket_path};

#[derive(Debug, Parser)]
#[command(name = "waldo", about = "Observe and control the active Niri desktop")]
pub struct Arguments {
    /// Override the private controller socket path.
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Logical client identity sent in the controller handshake.
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
    /// Execute an ActRequest JSON document, grounded on a fresh observation.
    ///
    /// An observation belongs to the connection that made it, so the batch is
    /// grounded on the ObserveRequest document's observation, taken on this
    /// invocation's connection; the document's own observation_id is replaced.
    Act {
        /// JSON document path, or '-' for standard input.
        document: PathBuf,
        /// ObserveRequest document used to ground this action on the same connection.
        #[arg(long)]
        observe: PathBuf,
        /// Cancel this batch through Stop after the given delay.
        #[arg(long, conflicts_with = "handover_to")]
        stop_after_ms: Option<u64>,
        /// Transfer the acquired driving lease after this batch completes.
        #[arg(long, conflicts_with = "stop_after_ms")]
        handover_to: Option<u64>,
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
            Self::Io(error) => write!(formatter, "could not read request document: {error}"),
            Self::Json(error) => write!(formatter, "could not encode the result: {error}"),
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
/// [`reports_failure`] says whether that value records a failed request.
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
        if document == Path::new("-") && observe == Path::new("-") {
            return Err(CliError::SharedStdin);
        }
        let mut request: ActRequest = match request_document(&document, RequestId::new(3))? {
            Ok(request) => request,
            Err(response) => return Ok(response),
        };
        let observation_request: ObserveRequest = match request_document(&observe, request_id)? {
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
            return json_envelope("act_stop", acted, "stop", stopped);
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
                .await;
            return json_envelope("act_handover", Ok(acted), "handover", handed_over);
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

/// Whether a value [`execute`] returned records a failed request: a
/// protocol error, a batch that did not complete, or a transport failure
/// inside a composed result. The CLI exits unsuccessfully for these after
/// printing the whole value.
pub fn reports_failure(value: &Value) -> bool {
    match value["type"].as_str() {
        Some("error") => true,
        Some("acted") => value["result"]["stop_reason"] != "completed",
        Some("act_stop") => {
            // Stop exists to cut the batch short, so a cancelled batch is the
            // expected outcome rather than a failure.
            let act = &value["act"];
            reports_failure(&value["stop"])
                || (act["type"] == "acted"
                    && !matches!(
                        act["result"]["stop_reason"].as_str(),
                        Some("completed" | "cancelled")
                    ))
                || act["type"] != "acted"
        }
        Some("act_handover") => {
            reports_failure(&value["act"]) || reports_failure(&value["handover"])
        }
        Some("transport_failure") => true,
        _ => false,
    }
}

fn read_document<T: DeserializeOwned>(
    path: &Path,
) -> Result<Result<T, serde_json::Error>, CliError> {
    let mut document = String::new();
    if path == Path::new("-") {
        io::stdin().read_to_string(&mut document)?;
    } else {
        document = fs::read_to_string(path)?;
    }
    Ok(serde_json::from_str(&document))
}

fn request_document<T: DeserializeOwned>(
    path: &Path,
    request_id: RequestId,
) -> Result<Result<T, Value>, CliError> {
    match read_document(path)? {
        Ok(request) => Ok(Ok(request)),
        Err(error) => Ok(Err(serde_json::to_value(ServerMessage::Error {
            request_id: Some(request_id),
            error: ProtocolError {
                code: ErrorCode::InvalidRequest,
                message: document_diagnostic(&error),
                retryable: false,
            },
        })?)),
    }
}

/// Says whether a request document is malformed JSON or the wrong shape,
/// and where.
fn document_diagnostic(error: &serde_json::Error) -> String {
    let kind = match error.classify() {
        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
            "request document is not valid JSON"
        }
        serde_json::error::Category::Data | serde_json::error::Category::Io => {
            "request document does not match the protocol schema"
        }
    };
    format!("{kind}: {}", decode_error_detail(error))
}

/// Composes an Act response with the request that followed it. A transport
/// failure on either keeps the other's response, so receipts already
/// received are never lost.
fn json_envelope(
    kind: &str,
    acted: Result<ServerMessage, TransportError>,
    following_name: &str,
    following: Result<ServerMessage, TransportError>,
) -> Result<Value, CliError> {
    let render = |response: Result<ServerMessage, TransportError>| match response {
        Ok(message) => serde_json::to_value(message),
        Err(error) => Ok(serde_json::json!({
            "type": "transport_failure",
            "message": error.to_string(),
        })),
    };
    Ok(serde_json::json!({
        "type": kind,
        "act": render(acted)?,
        (following_name): render(following)?,
    }))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Arguments, Command};

    #[test]
    fn act_needs_an_observation_taken_on_its_own_connection() {
        assert!(Arguments::try_parse_from(["waldo", "act", "batch.json"]).is_err());
    }

    #[test]
    fn act_can_ground_a_saved_batch_on_the_same_controller_connection() {
        let arguments =
            Arguments::try_parse_from(["waldo", "act", "batch.json", "--observe", "source.json"])
                .unwrap();

        assert!(matches!(
            arguments.command,
            Command::Act {
                observe: path,
                ..
            } if path == std::path::Path::new("source.json")
        ));
    }

    #[test]
    fn act_can_compose_stop_or_handover_without_changing_the_saved_document() {
        let cancelled = Arguments::try_parse_from([
            "waldo",
            "act",
            "batch.json",
            "--observe",
            "source.json",
            "--stop-after-ms",
            "25",
        ])
        .unwrap();
        let handed_over = Arguments::try_parse_from([
            "waldo",
            "act",
            "batch.json",
            "--observe",
            "source.json",
            "--handover-to",
            "7",
        ])
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
    fn failed_requests_and_batches_are_reported_as_failures() {
        use serde_json::json;

        let acted =
            |stop_reason: &str| json!({"type": "acted", "result": {"stop_reason": stop_reason}});
        let stopped = json!({"type": "stopped", "result": {"released_session": 1}});
        let lost = json!({"type": "transport_failure", "message": "connection closed"});

        assert!(!super::reports_failure(&json!({"type": "queried"})));
        assert!(super::reports_failure(&json!({"type": "error"})));
        assert!(!super::reports_failure(&acted("completed")));
        assert!(super::reports_failure(&acted("execution_failed")));
        assert!(super::reports_failure(&acted("verification_failed")));
        assert!(!super::reports_failure(
            &json!({"type": "act_stop", "act": acted("cancelled"), "stop": stopped})
        ));
        assert!(super::reports_failure(
            &json!({"type": "act_stop", "act": acted("execution_failed"), "stop": stopped})
        ));
        assert!(super::reports_failure(
            &json!({"type": "act_stop", "act": acted("cancelled"), "stop": lost})
        ));
        assert!(super::reports_failure(
            &json!({"type": "act_handover", "act": acted("completed"), "handover": lost})
        ));
    }

    #[test]
    fn document_diagnostics_say_whether_json_or_shape_failed() {
        let syntax = serde_json::from_str::<waldo_protocol::ActRequest>("{").unwrap_err();
        let shape = serde_json::from_str::<waldo_protocol::ActRequest>("{}").unwrap_err();

        assert!(
            super::document_diagnostic(&syntax).starts_with("request document is not valid JSON")
        );
        assert!(
            super::document_diagnostic(&shape)
                .starts_with("request document does not match the protocol schema")
        );
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
