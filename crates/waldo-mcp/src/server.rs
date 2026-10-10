use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::Mutex,
    task::JoinSet,
};
use waldo_protocol::{ClientId, ClientMessage, RequestId};
use waldo_socket::PipelinedControllerClient;

use crate::{
    McpError,
    result::{tool_failure, tool_response},
    tools::{CallToolParams, JsonRpcMessage, decode, initialize, rpc_error, tools},
};

/// One MCP process and its persistent connection to the session controller.
#[derive(Clone)]
pub struct McpServer {
    socket: Arc<PathBuf>,
    model_image_max_edge: NonZeroU32,
    client_id: ClientId,
    controller: Arc<Mutex<Option<PipelinedControllerClient>>>,
    next_request_id: Arc<AtomicU64>,
}

impl McpServer {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        let default = waldo_settings::Settings::from_sources(None, [], None)
            .expect("empty settings sources are valid")
            .model_image_max_edge()
            .expect("the declared image edge default is valid");
        Self::with_image_max_edge(socket, default)
    }

    /// Uses the operator-resolved limit for MCP image presentation.
    pub fn with_image_max_edge(
        socket: impl Into<PathBuf>,
        model_image_max_edge: NonZeroU32,
    ) -> Self {
        let client_id = ClientId::new(format!("waldo-mcp-{}", std::process::id()))
            .expect("a prefixed process id is never blank");
        Self {
            socket: Arc::new(socket.into()),
            model_image_max_edge,
            client_id,
            controller: Arc::new(Mutex::new(None)),
            next_request_id: Arc::new(AtomicU64::new(2)),
        }
    }

    /// Serves newline-delimited JSON-RPC until the client closes stdin.
    pub async fn run<R, W>(&self, reader: R, writer: W) -> Result<(), McpError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = BufWriter::new(writer);
        let mut requests = JoinSet::new();
        let mut input_open = true;

        while input_open || !requests.is_empty() {
            tokio::select! {
                line = lines.next_line(), if input_open => match line? {
                    Some(line) => match serde_json::from_str::<JsonRpcMessage>(&line) {
                        Ok(message) => {
                            let server = self.clone();
                            requests.spawn(async move { server.handle(message).await });
                        }
                        Err(error) => {
                            write_response(
                                &mut writer,
                                &rpc_error(Value::Null, -32700, error.to_string()),
                            ).await?;
                        }
                    },
                    None => input_open = false,
                },
                completed = requests.join_next(), if !requests.is_empty() => {
                    if let Some(response) = completed.expect("request set was not empty")
                        .map_err(std::io::Error::other)?
                    {
                        write_response(&mut writer, &response).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle(&self, message: JsonRpcMessage) -> Option<Value> {
        let id = message.id?;
        if message.jsonrpc != "2.0" {
            return Some(rpc_error(id, -32600, "jsonrpc must be \"2.0\""));
        }
        let result = match message.method.as_str() {
            "initialize" => initialize(message.params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call_tool(message.params).await,
            _ => return Some(rpc_error(id, -32601, "method not found")),
        };

        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => rpc_error(id, -32602, error),
        })
    }

    async fn call_tool(&self, params: Value) -> Result<Value, String> {
        let call: CallToolParams = decode(params)?;
        let request_id = self.next_controller_request_id();
        let message = match call.name.as_str() {
            "observe" => ClientMessage::Observe {
                request_id,
                request: match decode_tool_arguments(call.arguments, "ObserveRequest") {
                    Ok(request) => request,
                    Err(error) => return Ok(tool_failure(error)),
                },
            },
            "act" => ClientMessage::Act {
                request_id,
                request: match decode_tool_arguments(call.arguments, "ActRequest") {
                    Ok(request) => request,
                    Err(error) => return Ok(tool_failure(error)),
                },
            },
            "query" => ClientMessage::Query {
                request_id,
                request: match decode_tool_arguments(call.arguments, "QueryRequest") {
                    Ok(request) => request,
                    Err(error) => return Ok(tool_failure(error)),
                },
            },
            "stop" => ClientMessage::Stop {
                request_id,
                request: match decode_tool_arguments(call.arguments, "StopRequest") {
                    Ok(request) => request,
                    Err(error) => return Ok(tool_failure(error)),
                },
            },
            _ => return Err(format!("unknown Waldo tool: {}", call.name)),
        };

        let response = match self.controller().await {
            Ok(controller) => controller.request(message).await,
            Err(error) => return Ok(crate::result::transport_failure(error.to_string())),
        };
        match response {
            Ok(response) => Ok(tool_response(response, self.model_image_max_edge)),
            Err(error) => {
                *self.controller.lock().await = None;
                Ok(crate::result::transport_failure(error.to_string()))
            }
        }
    }

    async fn controller(&self) -> Result<PipelinedControllerClient, McpError> {
        let mut controller = self.controller.lock().await;
        if controller.is_none() {
            *controller = Some(
                PipelinedControllerClient::connect(
                    Path::new(self.socket.as_ref()),
                    self.client_id.clone(),
                )
                .await?,
            );
        }
        Ok(controller
            .as_ref()
            .expect("controller was connected")
            .clone())
    }

    fn next_controller_request_id(&self) -> RequestId {
        RequestId::new(self.next_request_id.fetch_add(1, Ordering::Relaxed))
    }
}

fn decode_tool_arguments<T: serde::de::DeserializeOwned>(
    value: Value,
    schema: &str,
) -> Result<T, waldo_protocol::ProtocolError> {
    decode(value).map_err(|_| waldo_protocol::ProtocolError {
        code: waldo_protocol::ErrorCode::InvalidRequest,
        message: format!("tool arguments do not match the {schema} schema"),
        retryable: false,
    })
}

async fn write_response<W>(writer: &mut BufWriter<W>, response: &Value) -> Result<(), McpError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&serde_json::to_vec(response)?).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::McpServer;

    #[tokio::test]
    async fn stdio_negotiates_and_lists_tools_without_a_controller() {
        let (mut client_write, server_read) = tokio::io::duplex(16 * 1024);
        let (server_write, mut client_read) = tokio::io::duplex(128 * 1024);
        let server = McpServer::new("/controller-is-not-needed");
        let task = tokio::spawn(async move { server.run(server_read, server_write).await });

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        client_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            )
            .await
            .unwrap();
        drop(client_write);

        let mut lines = BufReader::new(&mut client_read).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let initialized = [&first, &second]
            .into_iter()
            .find(|response| response["id"] == 1)
            .unwrap();
        let listed = [&first, &second]
            .into_iter()
            .find(|response| response["id"] == 2)
            .unwrap();

        assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 4);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stdio_rejects_a_request_that_is_not_json_rpc_2() {
        let (mut client_write, server_read) = tokio::io::duplex(16 * 1024);
        let (server_write, mut client_read) = tokio::io::duplex(16 * 1024);
        let server = McpServer::new("/controller-is-not-needed");
        let task = tokio::spawn(async move { server.run(server_read, server_write).await });

        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        client_write
            .write_all(b"{\"jsonrpc\":\"1.0\",\"id\":7,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        drop(client_write);

        let mut lines = BufReader::new(&mut client_read).lines();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();

        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32600);
        assert!(response.get("result").is_none());
        task.await.unwrap().unwrap();
    }
}
