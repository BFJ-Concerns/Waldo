//! Length-delimited JSON transport for Waldo's private controller socket.

use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
    sync::{Mutex, mpsc, oneshot},
};
use waldo_protocol::{
    Capabilities, ClientId, ClientMessage, ConnectionId, RequestId, SCHEMA_MAJOR_VERSION,
    ServerMessage,
};
use waldo_settings::{Settings, SettingsError};

/// Maximum accepted JSON payload, excluding the four-byte length prefix.
///
/// This covers a worst-case base64-encoded 3840×2160 RGBA observation while
/// retaining a firm allocation bound for malformed local clients.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Json(serde_json::Error),
    FrameTooLarge {
        size: usize,
        maximum: usize,
    },
    ConnectionClosed,
    Handshake(String),
    ResponseMismatch {
        expected: RequestId,
        received: Option<RequestId>,
    },
    Settings(SettingsError),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "controller socket I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "controller message is invalid JSON: {error}"),
            Self::FrameTooLarge { size, maximum } => {
                write!(
                    formatter,
                    "controller frame is {size} bytes; maximum is {maximum}"
                )
            }
            Self::ConnectionClosed => formatter.write_str("controller socket closed"),
            Self::Handshake(message) => write!(formatter, "controller handshake failed: {message}"),
            Self::ResponseMismatch { expected, received } => write!(
                formatter,
                "controller response id {received:?} does not match request {}",
                expected.get()
            ),
            Self::Settings(error) => write!(formatter, "controller socket settings: {error}"),
        }
    }
}

/// Returns the controller socket the operator's settings name.
pub fn default_socket_path() -> Result<PathBuf, TransportError> {
    Ok(Settings::load()?.socket_path()?)
}

impl From<SettingsError> for TransportError {
    fn from(error: SettingsError) -> Self {
        Self::Settings(error)
    }
}

impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Settings(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for TransportError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// Writes one bounded, length-delimited JSON value.
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(message)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge {
            size: payload.len(),
            maximum: MAX_FRAME_BYTES,
        });
    }

    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one bounded, length-delimited JSON value.
pub async fn read_message<R, T>(reader: &mut R) -> Result<T, TransportError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let payload = read_frame(reader).await?;
    Ok(serde_json::from_slice(&payload)?)
}

/// Reads one bounded frame without interpreting its JSON payload.
///
/// The controller service uses this lower-level boundary so a recoverably
/// framed request which does not match the current schema can receive a
/// structured protocol error without closing the healthy connection.
pub async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, TransportError>
where
    R: AsyncRead + Unpin,
{
    let size = match reader.read_u32().await {
        Ok(size) => size as usize,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(TransportError::ConnectionClosed);
        }
        Err(error) => return Err(TransportError::Io(error)),
    };
    if size > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge {
            size,
            maximum: MAX_FRAME_BYTES,
        });
    }

    let mut payload = vec![0; size];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

/// A connected and schema-checked controller client.
pub struct ControllerClient {
    stream: UnixStream,
    connection_id: ConnectionId,
    capabilities: Capabilities,
}

struct PendingRequest {
    message: ClientMessage,
    response: oneshot::Sender<Result<ServerMessage, String>>,
}

type PendingResponses =
    std::sync::Arc<Mutex<HashMap<RequestId, oneshot::Sender<Result<ServerMessage, String>>>>>;

/// A cloneable controller connection which safely pipelines requests.
///
/// This is required by bindings whose Stop request must be able to overtake a
/// long-running Act request on the same lease-owning connection.
#[derive(Clone)]
pub struct PipelinedControllerClient {
    requests: mpsc::Sender<PendingRequest>,
    connection_id: ConnectionId,
    capabilities: Capabilities,
}

impl PipelinedControllerClient {
    pub async fn connect(path: &Path, client_id: ClientId) -> Result<Self, TransportError> {
        let client = ControllerClient::connect(path, client_id).await?;
        let ControllerClient {
            stream,
            connection_id,
            capabilities,
        } = client;
        let (reader, writer) = stream.into_split();
        let pending = std::sync::Arc::new(Mutex::new(HashMap::new()));
        let (request_tx, request_rx) = mpsc::channel(16);

        tokio::spawn(write_requests(writer, request_rx, pending.clone()));
        tokio::spawn(read_responses(reader, pending));

        Ok(Self {
            requests: request_tx,
            connection_id,
            capabilities,
        })
    }

    pub const fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub async fn request(&self, message: ClientMessage) -> Result<ServerMessage, TransportError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(PendingRequest {
                message,
                response: response_tx,
            })
            .await
            .map_err(|_| TransportError::ConnectionClosed)?;
        match response_rx.await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(TransportError::Handshake(error)),
            Err(_) => Err(TransportError::ConnectionClosed),
        }
    }
}

async fn write_requests(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut requests: mpsc::Receiver<PendingRequest>,
    pending: PendingResponses,
) {
    while let Some(request) = requests.recv().await {
        let request_id = request.message.request_id();
        pending.lock().await.insert(request_id, request.response);
        if let Err(error) = write_message(&mut writer, &request.message).await {
            fail_pending(&pending, error.to_string()).await;
            return;
        }
    }
}

async fn read_responses(mut reader: tokio::net::unix::OwnedReadHalf, pending: PendingResponses) {
    loop {
        match read_message::<_, ServerMessage>(&mut reader).await {
            Ok(response) => {
                let Some(request_id) = response.request_id() else {
                    continue;
                };
                if let Some(sender) = pending.lock().await.remove(&request_id) {
                    let _ = sender.send(Ok(response));
                }
            }
            Err(error) => {
                fail_pending(&pending, error.to_string()).await;
                return;
            }
        }
    }
}

async fn fail_pending(
    pending: &Mutex<HashMap<RequestId, oneshot::Sender<Result<ServerMessage, String>>>>,
    error: String,
) {
    for (_, sender) in pending.lock().await.drain() {
        let _ = sender.send(Err(error.clone()));
    }
}

impl ControllerClient {
    pub async fn connect(path: &Path, client_id: ClientId) -> Result<Self, TransportError> {
        let mut stream = UnixStream::connect(path).await?;
        let request_id = RequestId::new(1);
        write_message(
            &mut stream,
            &ClientMessage::Hello {
                request_id,
                schema_major: SCHEMA_MAJOR_VERSION,
                client_id,
            },
        )
        .await?;

        match read_message(&mut stream).await? {
            ServerMessage::Connected {
                request_id: received,
                connection_id,
                capabilities,
            } if received == request_id => Ok(Self {
                stream,
                connection_id,
                capabilities,
            }),
            ServerMessage::Error { error, .. } => Err(TransportError::Handshake(error.message)),
            other => Err(TransportError::Handshake(format!(
                "unexpected response: {other:?}"
            ))),
        }
    }

    pub const fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    pub const fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub async fn request(
        &mut self,
        message: &ClientMessage,
    ) -> Result<ServerMessage, TransportError> {
        let expected = message.request_id();
        write_message(&mut self.stream, message).await?;
        let response: ServerMessage = read_message(&mut self.stream).await?;
        if response.request_id() != Some(expected) {
            return Err(TransportError::ResponseMismatch {
                expected,
                received: response.request_id(),
            });
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::{MAX_FRAME_BYTES, TransportError, read_frame, read_message, write_message};

    #[tokio::test]
    async fn messages_round_trip_over_an_async_stream() {
        let (mut writer, mut reader) = duplex(128);
        write_message(&mut writer, &vec!["observe", "act"])
            .await
            .unwrap();

        let decoded: Vec<String> = read_message(&mut reader).await.unwrap();

        assert_eq!(decoded, ["observe", "act"]);
    }

    #[tokio::test]
    async fn raw_frame_reading_preserves_recoverable_invalid_json() {
        let (mut writer, mut reader) = duplex(128);
        writer.write_u32(9).await.unwrap();
        writer.write_all(b"{not-json").await.unwrap();

        assert_eq!(read_frame(&mut reader).await.unwrap(), b"{not-json");
    }

    #[tokio::test]
    async fn oversized_incoming_frames_are_rejected_before_allocation() {
        let (mut writer, mut reader) = duplex(16);
        writer
            .write_u32((MAX_FRAME_BYTES + 1) as u32)
            .await
            .unwrap();

        let error = read_message::<_, serde_json::Value>(&mut reader)
            .await
            .unwrap_err();

        assert!(matches!(error, TransportError::FrameTooLarge { .. }));
    }

    #[tokio::test]
    async fn a_cleanly_closed_socket_has_a_distinct_error() {
        let (writer, mut reader) = duplex(16);
        drop(writer);

        let error = read_message::<_, serde_json::Value>(&mut reader)
            .await
            .unwrap_err();

        assert!(matches!(error, TransportError::ConnectionClosed));
    }

    #[test]
    fn frame_limit_covers_an_uncompressed_target_output_after_base64_expansion() {
        let rgba_bytes: usize = 3840 * 2160 * 4;
        let base64_ceiling = rgba_bytes.div_ceil(3) * 4;

        assert!(MAX_FRAME_BYTES > base64_ceiling);
    }
}
