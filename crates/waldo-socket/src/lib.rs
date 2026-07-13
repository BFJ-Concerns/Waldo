//! Length-delimited JSON transport for Waldo's private controller socket.

use std::{
    env,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::UnixStream,
};
use waldo_protocol::{
    Capabilities, ClientId, ClientMessage, ConnectionId, RequestId, SCHEMA_MAJOR_VERSION,
    ServerMessage,
};

/// Maximum accepted JSON payload, excluding the four-byte length prefix.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

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
    RuntimeDirectoryMissing,
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
            Self::RuntimeDirectoryMissing => {
                formatter.write_str("XDG_RUNTIME_DIR is not set; cannot locate the private socket")
            }
        }
    }
}

/// Returns the controller socket beneath the user's private runtime directory.
pub fn default_socket_path() -> Result<PathBuf, TransportError> {
    let runtime = env::var_os("XDG_RUNTIME_DIR").ok_or(TransportError::RuntimeDirectoryMissing)?;
    Ok(PathBuf::from(runtime).join("waldo/controller.sock"))
}

impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
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
    Ok(serde_json::from_slice(&payload)?)
}

/// A connected and schema-checked controller client.
pub struct ControllerClient {
    stream: UnixStream,
    connection_id: ConnectionId,
    capabilities: Capabilities,
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

    use super::{MAX_FRAME_BYTES, TransportError, read_message, write_message};

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
}
