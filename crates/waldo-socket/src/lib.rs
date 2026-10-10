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
    ClientId, ClientMessage, ConnectionId, RequestId, SCHEMA_MAJOR_VERSION, ServerMessage,
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
    /// EOF after at least one byte of the four-byte frame prefix.
    TruncatedFrame,
    /// The request may have executed, but its response was lost.
    OutcomeUnknown {
        reason: String,
    },
    Handshake(String),
    /// A request was not dispatched on a failed connection.
    ConnectionFailed(String),
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
            Self::Json(error) => match error.classify() {
                serde_json::error::Category::Data => write!(
                    formatter,
                    "controller message does not match the protocol schema: {error}"
                ),
                _ => write!(formatter, "controller message is invalid JSON: {error}"),
            },
            Self::FrameTooLarge { size, maximum } => {
                write!(
                    formatter,
                    "controller frame is {size} bytes; maximum is {maximum}"
                )
            }
            Self::ConnectionClosed => formatter.write_str("controller socket closed"),
            Self::TruncatedFrame => {
                formatter.write_str("controller frame length prefix was truncated")
            }
            Self::OutcomeUnknown { reason } => write!(
                formatter,
                "the controller connection failed after the request was sent: {reason}"
            ),
            Self::Handshake(message) => write!(formatter, "controller handshake failed: {message}"),
            Self::ConnectionFailed(message) => {
                write!(formatter, "controller connection failed: {message}")
            }
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
    write_message_with_progress(writer, message)
        .await
        .map_err(|failure| failure.error)
}

/// Whether the peer could have decoded the frame when writing failed.
struct FrameWriteFailure {
    error: TransportError,
    frame_complete: bool,
}

impl From<TransportError> for FrameWriteFailure {
    fn from(error: TransportError) -> Self {
        Self {
            error,
            frame_complete: false,
        }
    }
}

impl From<io::Error> for FrameWriteFailure {
    fn from(error: io::Error) -> Self {
        TransportError::Io(error).into()
    }
}

async fn write_message_with_progress<W, T>(
    writer: &mut W,
    message: &T,
) -> Result<(), FrameWriteFailure>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(message).map_err(TransportError::Json)?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge {
            size: payload.len(),
            maximum: MAX_FRAME_BYTES,
        }
        .into());
    }
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await.map_err(|error| FrameWriteFailure {
        error: TransportError::Io(error),
        frame_complete: true,
    })?;
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
    let mut prefix = [0; 4];
    if reader.read(&mut prefix[..1]).await? == 0 {
        return Err(TransportError::ConnectionClosed);
    }
    match reader.read_exact(&mut prefix[1..]).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(TransportError::TruncatedFrame);
        }
        Err(error) => return Err(TransportError::Io(error)),
    }
    let size = u32::from_be_bytes(prefix) as usize;
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
}

type ResponseSender = oneshot::Sender<Result<ServerMessage, TransportError>>;

struct PendingRequest {
    message: ClientMessage,
    response: ResponseSender,
}

/// What both pipelined workers share: the callers awaiting a response, and
/// the failure that ended the connection, once one has.
#[derive(Default)]
struct ConnectionState {
    pending: HashMap<RequestId, ResponseSender>,
    failure: Option<String>,
}

type SharedConnection = std::sync::Arc<Mutex<ConnectionState>>;

impl ConnectionState {
    /// A reader failure cannot prove any pending request was not dispatched.
    fn fail(&mut self, reason: String) {
        let reason = self.failure.get_or_insert(reason).clone();
        for (_, sender) in self.pending.drain() {
            let _ = sender.send(Err(TransportError::OutcomeUnknown {
                reason: reason.clone(),
            }));
        }
    }
}

/// A cloneable controller connection which safely pipelines requests.
///
/// This is required by bindings whose Stop request must be able to overtake a
/// long-running Act request on the same lease-owning connection. Once the
/// connection fails, every queued and later request fails with that reason.
#[derive(Clone)]
pub struct PipelinedControllerClient {
    requests: mpsc::Sender<PendingRequest>,
    connection: SharedConnection,
}

impl PipelinedControllerClient {
    pub async fn connect(path: &Path, client_id: ClientId) -> Result<Self, TransportError> {
        let ControllerClient { stream, .. } = ControllerClient::connect(path, client_id).await?;
        let (reader, writer) = stream.into_split();
        let connection = SharedConnection::default();
        let (request_tx, request_rx) = mpsc::channel(16);

        tokio::spawn(write_requests(writer, request_rx, connection.clone()));
        tokio::spawn(read_responses(reader, connection.clone()));

        Ok(Self {
            requests: request_tx,
            connection,
        })
    }

    pub async fn request(&self, message: ClientMessage) -> Result<ServerMessage, TransportError> {
        let (response_tx, response_rx) = oneshot::channel();
        let sent = self
            .requests
            .send(PendingRequest {
                message,
                response: response_tx,
            })
            .await;
        match (sent, response_rx.await) {
            (Ok(()), Ok(response)) => response,
            _ => Err(self.recorded_failure().await),
        }
    }

    async fn recorded_failure(&self) -> TransportError {
        match &self.connection.lock().await.failure {
            Some(reason) => TransportError::ConnectionFailed(reason.clone()),
            None => TransportError::ConnectionClosed,
        }
    }
}

async fn write_requests<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut requests: mpsc::Receiver<PendingRequest>,
    connection: SharedConnection,
) {
    while let Some(request) = requests.recv().await {
        let request_id = request.message.request_id();
        {
            let mut state = connection.lock().await;
            if let Some(reason) = &state.failure {
                let reason = TransportError::ConnectionFailed(reason.clone());
                let _ = request.response.send(Err(reason));
                continue;
            }
            match state.pending.entry(request_id) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    let _ = request
                        .response
                        .send(Err(TransportError::ConnectionFailed(format!(
                            "request id {} is already awaiting a response on this connection",
                            request_id.get()
                        ))));
                    continue;
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(request.response);
                }
            }
        }
        if let Err(failure) = write_message_with_progress(&mut writer, &request.message).await {
            let mut state = connection.lock().await;
            let reason = failure.error.to_string();
            if !failure.frame_complete
                && let Some(sender) = state.pending.remove(&request_id)
            {
                let _ = sender.send(Err(TransportError::ConnectionFailed(reason.clone())));
            }
            state.fail(reason);
        }
    }
}

async fn read_responses(mut reader: tokio::net::unix::OwnedReadHalf, connection: SharedConnection) {
    loop {
        let response = match read_message::<_, ServerMessage>(&mut reader).await {
            Ok(response) => response,
            Err(error) => {
                connection.lock().await.fail(error.to_string());
                return;
            }
        };
        let mut state = connection.lock().await;
        let sender = response
            .request_id()
            .and_then(|request_id| state.pending.remove(&request_id));
        match sender {
            Some(sender) => {
                let _ = sender.send(Ok(response));
            }
            None => {
                // A response nobody awaits means the two ends no longer agree
                // on the conversation, so nothing later can be trusted.
                let reason = match response {
                    ServerMessage::Error { error, .. } => {
                        format!("controller reported a connection error: {}", error.message)
                    }
                    other => format!(
                        "controller answered request id {:?}, which no request is awaiting",
                        other.request_id().map(RequestId::get)
                    ),
                };
                state.fail(reason);
                return;
            }
        }
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
                ..
            } if received == request_id => Ok(Self {
                stream,
                connection_id,
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

    /// A controller that completes the handshake, records each request it
    /// reads, and answers with whatever `respond` returns for it.
    async fn scripted_controller(
        respond: impl Fn(&super::ClientMessage) -> Option<super::ServerMessage> + Send + 'static,
    ) -> std::path::PathBuf {
        use waldo_protocol::{Capabilities, ConnectionId, ServerMessage};
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let directory = std::env::temp_dir().join(format!(
            "waldo-socket-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("controller.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello: super::ClientMessage = read_message(&mut stream).await.unwrap();
            write_message(
                &mut stream,
                &ServerMessage::Connected {
                    request_id: hello.request_id(),
                    connection_id: ConnectionId::new(1),
                    capabilities: Capabilities::unavailable(),
                },
            )
            .await
            .unwrap();
            while let Ok(request) = read_message::<_, super::ClientMessage>(&mut stream).await {
                if let Some(response) = respond(&request) {
                    write_message(&mut stream, &response).await.unwrap();
                }
            }
        });
        path
    }

    fn query(id: u64) -> super::ClientMessage {
        super::ClientMessage::Query {
            request_id: waldo_protocol::RequestId::new(id),
            request: waldo_protocol::QueryRequest::default(),
        }
    }

    #[tokio::test]
    async fn a_stray_response_fails_waiting_and_later_requests_with_its_reason() {
        let path = scripted_controller(|_| {
            Some(waldo_protocol::ServerMessage::Error {
                request_id: None,
                error: waldo_protocol::ProtocolError {
                    code: waldo_protocol::ErrorCode::Internal,
                    message: "controller lost track of the connection".into(),
                    retryable: false,
                },
            })
        })
        .await;
        let client = super::PipelinedControllerClient::connect(
            &path,
            waldo_protocol::ClientId::new("stray-response-test").unwrap(),
        )
        .await
        .unwrap();
        let deadline = std::time::Duration::from_secs(5);

        for id in [2, 3] {
            let error = tokio::time::timeout(deadline, client.request(query(id)))
                .await
                .expect("a request on a failed connection must not wait forever")
                .unwrap_err();
            let reason = match error {
                TransportError::OutcomeUnknown { reason } if id == 2 => reason,
                TransportError::ConnectionFailed(reason) if id == 3 => reason,
                error => panic!("request {id} had the wrong dispatch certainty: {error}"),
            };
            assert!(reason.contains("lost track of the connection"), "{reason}");
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_duplicate_in_flight_request_id_is_refused() {
        // The controller never answers, so the first request stays in flight.
        let path = scripted_controller(|_| None).await;
        let client = super::PipelinedControllerClient::connect(
            &path,
            waldo_protocol::ClientId::new("duplicate-id-test").unwrap(),
        )
        .await
        .unwrap();
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.request(query(2)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let error =
            tokio::time::timeout(std::time::Duration::from_secs(5), client.request(query(2)))
                .await
                .expect("a duplicate request must be refused at once")
                .unwrap_err();

        assert!(
            error.to_string().contains("already awaiting a response"),
            "{error}"
        );
        assert!(!first.is_finished());
        first.abort();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn partial_length_prefixes_are_framing_errors() {
        for length in 1..=3 {
            let mut bytes = &b"\0\0\0"[..length];
            let error = read_frame(&mut bytes).await.unwrap_err();
            assert!(
                matches!(error, TransportError::TruncatedFrame),
                "{length}: {error}"
            );
        }
    }

    /// A byte sink that fails at a chosen byte boundary or at flush. It never
    /// fabricates a transport classification: the real writer decides that.
    struct FailingWriter {
        bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        limit: usize,
        fail_flush: bool,
    }

    impl tokio::io::AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let mut written = self.bytes.lock().unwrap();
            let count = bytes.len().min(self.limit.saturating_sub(written.len()));
            if count == 0 {
                return std::task::Poll::Ready(Err(std::io::Error::from(
                    std::io::ErrorKind::BrokenPipe,
                )));
            }
            written.extend_from_slice(&bytes[..count]);
            std::task::Poll::Ready(Ok(count))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(if self.fail_flush {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            } else {
                Ok(())
            })
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    async fn failed_write(
        limit: usize,
        fail_flush: bool,
    ) -> (TransportError, TransportError, Vec<u8>) {
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let (first_tx, first_rx) = tokio::sync::oneshot::channel();
        let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
        tx.send(super::PendingRequest {
            message: query(2),
            response: first_tx,
        })
        .await
        .unwrap();
        tx.send(super::PendingRequest {
            message: query(3),
            response: queued_tx,
        })
        .await
        .unwrap();
        drop(tx);
        super::write_requests(
            FailingWriter {
                bytes: bytes.clone(),
                limit,
                fail_flush,
            },
            rx,
            super::SharedConnection::default(),
        )
        .await;
        let first = first_rx.await.unwrap().unwrap_err();
        let queued = queued_rx.await.unwrap().unwrap_err();
        let written = bytes.lock().unwrap().clone();
        (first, queued, written)
    }

    #[tokio::test]
    async fn incomplete_prefix_and_body_writes_are_not_dispatched() {
        for limit in [2, 5] {
            let (first, queued, bytes) = failed_write(limit, false).await;
            assert!(
                read_message::<_, super::ClientMessage>(&mut bytes.as_slice())
                    .await
                    .is_err()
            );
            assert!(
                matches!(first, TransportError::ConnectionFailed(_)),
                "{first}"
            );
            assert!(
                matches!(queued, TransportError::ConnectionFailed(_)),
                "{queued}"
            );
        }
    }

    #[tokio::test]
    async fn a_flush_failure_after_a_complete_frame_has_an_unknown_outcome() {
        let (first, queued, bytes) = failed_write(usize::MAX, true).await;
        let decoded: super::ClientMessage = read_message(&mut bytes.as_slice()).await.unwrap();
        assert_eq!(decoded, query(2));
        assert!(
            matches!(first, TransportError::OutcomeUnknown { .. }),
            "{first}"
        );
        assert!(
            matches!(queued, TransportError::ConnectionFailed(_)),
            "{queued}"
        );
    }

    #[tokio::test]
    async fn reader_failure_treats_even_unwritten_pending_requests_as_unknown() {
        let mut state = super::ConnectionState::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        state.pending.insert(waldo_protocol::RequestId::new(2), tx);
        state.fail("reader lost the connection".into());
        let error = rx.await.unwrap().unwrap_err();
        assert!(
            matches!(error, TransportError::OutcomeUnknown { .. }),
            "{error}"
        );
    }

    #[test]
    fn frame_limit_covers_an_uncompressed_target_output_after_base64_expansion() {
        let rgba_bytes: usize = 3840 * 2160 * 4;
        let base64_ceiling = rgba_bytes.div_ceil(3) * 4;

        assert!(MAX_FRAME_BYTES > base64_ceiling);
    }
}
