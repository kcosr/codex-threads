use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

const HANDSHAKE_URL: &str = "ws://localhost/rpc";
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

pub struct RpcClient {
    stream: WebSocketStream<UnixStream>,
    next_id: i64,
}

impl RpcClient {
    pub async fn connect(path: &Path) -> Result<Self> {
        let request = HANDSHAKE_URL
            .into_client_request()
            .context("invalid UDS websocket handshake URL")?;
        let unix = tokio::time::timeout(Duration::from_secs(10), UnixStream::connect(path))
            .await
            .context("timed out connecting to app-server UDS")?
            .with_context(|| format!("failed to connect to app-server UDS `{}`", path.display()))?;
        let (stream, _) = tokio::time::timeout(
            Duration::from_secs(10),
            client_async_with_config(request, unix, None),
        )
        .await
        .context("timed out upgrading UDS connection to websocket")?
        .context("failed to upgrade UDS connection to websocket")?;

        let mut client = Self { stream, next_id: 1 };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&mut self) -> Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "codex-threads",
                        "title": "codex-threads",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": {
                        "experimentalApi": true
                    }
                }),
                |_| {},
            )
            .await?;
        let _ = result;
        self.send_notification("initialized", Value::Null).await?;
        Ok(())
    }

    pub async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let mut message = json!({ "method": method });
        if !params.is_null() {
            message["params"] = params;
        }
        self.stream
            .send(Message::Text(message.to_string().into()))
            .await
            .context("failed to send notification")?;
        Ok(())
    }

    pub async fn request<F>(
        &mut self,
        method: &str,
        params: Value,
        mut on_notification: F,
    ) -> Result<Value>
    where
        F: FnMut(Notification),
    {
        let id = self.next_id;
        self.next_id += 1;
        let request = if params.is_null() {
            json!({ "id": id, "method": method })
        } else {
            json!({ "id": id, "method": method, "params": params })
        };
        self.stream
            .send(Message::Text(request.to_string().into()))
            .await
            .with_context(|| format!("failed to send `{method}` request"))?;

        loop {
            let next = tokio::time::timeout(REQUEST_READ_TIMEOUT, self.stream.next())
                .await
                .with_context(|| format!("timed out waiting for app-server `{method}` response"))?;
            let Some(message) = next else {
                return Err(anyhow!(
                    "app-server connection closed while waiting for `{method}`"
                ));
            };
            let message = message.context("failed to read websocket message")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("app-server sent invalid JSON: {text}"))?;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = value.get("error") {
                    let error = parse_rpc_error(error);
                    return Err(anyhow!("{}", format_rpc_error(method, &error)));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            if let Some(method) = value.get("method").and_then(Value::as_str) {
                if value.get("id").is_some() {
                    self.reject_server_request(&value).await?;
                } else {
                    on_notification(Notification {
                        method: method.to_string(),
                        params: value.get("params").cloned().unwrap_or(Value::Null),
                    });
                }
            }
        }
    }

    pub async fn next_notification_or_request(&mut self) -> Result<Notification> {
        loop {
            let Some(message) = self.stream.next().await else {
                return Err(anyhow!("app-server connection closed"));
            };
            let message = message.context("failed to read websocket message")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("app-server sent invalid JSON: {text}"))?;
            if value.get("id").is_some() && value.get("method").is_some() {
                self.reject_server_request(&value).await?;
                continue;
            }
            if let Some(method) = value.get("method").and_then(Value::as_str) {
                return Ok(Notification {
                    method: method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }

    async fn reject_server_request(&mut self, request: &Value) -> Result<()> {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let response = json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("unsupported server request `{method}`")
            }
        });
        self.stream
            .send(Message::Text(response.to_string().into()))
            .await
            .context("failed to reject unsupported server request")?;
        Ok(())
    }
}

fn parse_rpc_error(error: &Value) -> RpcError {
    RpcError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown app-server error")
            .to_string(),
    }
}

pub fn format_rpc_error(method: &str, error: &RpcError) -> String {
    if error.message.contains("experimentalApi") {
        format!("app-server rejected `{method}` because it requires experimentalApi capability")
    } else {
        format!(
            "app-server `{method}` error {}: {}",
            error.code, error.message
        )
    }
}
