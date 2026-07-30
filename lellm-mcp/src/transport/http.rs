//! HTTP Transport — Streamable HTTP 传输层。
//!
//! 架构：
//! - connect() 建立连接（无状态，仅初始化 reqwest Client）
//! - request() 通过 HTTP POST 发送 JSON-RPC 请求，等待响应
//! - 自动携带 MCP-Protocol-Version、Mcp-Method、Mcp-Name 标准 Headers
//! - 支持 application/json 与 text/event-stream 两种响应格式
//!
//! 参考：https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;

use super::{ConnectionState, McpTransport, TransportCapabilities};
use crate::protocol::{
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError, TransportError,
};

/// 通知 channel 容量。
const NOTIFICATION_BUFFER: usize = 64;

/// 默认请求超时（秒）。
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// 默认协议版本（MCP 2026-07-28）。
const DEFAULT_PROTOCOL_VERSION: &str = "2026-07-28";

/// HTTP Transport 配置。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// HTTP 端点 URL（如 https://mcp.map.qq.com/mcp?key=xxx&format=0）
    pub endpoint_url: String,
    /// 单次请求超时（默认 30 秒）。
    pub request_timeout: std::time::Duration,
    /// MCP 协议版本，默认 `2026-07-28`。
    pub protocol_version: String,
}

impl HttpConfig {
    pub fn new(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            request_timeout: std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            protocol_version: DEFAULT_PROTOCOL_VERSION.to_string(),
        }
    }

    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// 设置 MCP 协议版本。
    pub fn with_protocol_version(mut self, version: impl Into<String>) -> Self {
        self.protocol_version = version.into();
        self
    }
}

/// HTTP Transport 实现。
pub struct HttpTransport {
    config: HttpConfig,
    inner: Option<Arc<HttpTransportInner>>,
    state: watch::Sender<ConnectionState>,
    /// 持有 watch channel 的 receiver，确保 sender 始终有 subscriber，send() 才能更新值。
    #[allow(dead_code)]
    _state_rx: watch::Receiver<ConnectionState>,
}

struct HttpTransportInner {
    client: reqwest::Client,
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
}

impl HttpTransport {
    pub fn new(config: HttpConfig) -> Self {
        let (tx, rx) = watch::channel(ConnectionState::Disconnected);
        Self {
            config,
            inner: None,
            state: tx,
            _state_rx: rx,
        }
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn connect(&mut self) -> Result<(), McpError> {
        self.state.send(ConnectionState::Connecting).ok();

        let client = reqwest::Client::new();
        let (notification_tx, _) =
            tokio::sync::broadcast::channel::<JsonRpcNotification>(NOTIFICATION_BUFFER);

        self.inner = Some(Arc::new(HttpTransportInner {
            client,
            notification_tx,
        }));

        self.state.send(ConnectionState::Ready).ok();
        Ok(())
    }

    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let inner = self.inner.as_ref().ok_or_else(McpError::disconnected)?;

        let json = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;

        // 构建请求，携带 Streamable HTTP 标准 Headers
        let mut builder = inner
            .client
            .post(&self.config.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", &self.config.protocol_version)
            .header("Mcp-Method", &req.method_name)
            .timeout(self.config.request_timeout);

        // Mcp-Name header（tools/call → params.name; resources/read, prompts/get → params.uri）
        if let Some(name) = extract_mcp_name(&req.method_name, &req.params) {
            builder = builder.header("Mcp-Name", &name);
        }

        let response = builder
            .body(json)
            .send()
            .await
            .map_err(|e| McpError::Transport(TransportError::Http(e.to_string())))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(McpError::Transport(TransportError::Http(format!(
                "HTTP {}: {}",
                status, body
            ))));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            let bytes = response
                .bytes()
                .await
                .map_err(|e| McpError::Transport(TransportError::Http(e.to_string())))?;
            let body = String::from_utf8_lossy(&bytes);

            // SSE 格式: event:xxx\ndata:xxx\n\n (冒号后可能有空格)
            let mut current_event = String::new();
            let mut current_data = String::new();

            // 辅助：处理一个完整的 SSE 帧
            let process_frame = |event: &str,
                                 data: &str,
                                 req_id: u64|
             -> Option<Result<JsonRpcResponse, McpError>> {
                if event != "message" || data.is_empty() {
                    return None;
                }
                let Ok(msg) = serde_json::from_str::<crate::protocol::JsonRpcMessage>(data) else {
                    return None;
                };
                match msg {
                    crate::protocol::JsonRpcMessage::Response(resp) => {
                        if resp.id != req_id {
                            Some(Err(McpError::Protocol(format!(
                                "Response ID mismatch: expected {}, got {}",
                                req_id, resp.id
                            ))))
                        } else {
                            Some(Ok(resp))
                        }
                    }
                    crate::protocol::JsonRpcMessage::Notification(notif) => {
                        let _ = inner.notification_tx.send(notif);
                        None
                    }
                    _ => None,
                }
            };

            for line in body.lines() {
                if line.starts_with("event:") || line.starts_with("event: ") {
                    current_event = line.trim_start_matches("event:").trim().to_string();
                } else if line.starts_with("data:") || line.starts_with("data: ") {
                    current_data = line.trim_start_matches("data:").trim().to_string();
                } else if line.is_empty() && !current_data.is_empty() {
                    if let Some(result) = process_frame(&current_event, &current_data, req.id) {
                        return result;
                    }
                    current_event.clear();
                    current_data.clear();
                }
            }

            // Flush: 处理最后一个帧（可能没有尾随空行）
            if !current_data.is_empty()
                && let Some(result) = process_frame(&current_event, &current_data, req.id)
            {
                return result;
            }

            Err(McpError::Protocol("No response in SSE stream".to_string()))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| McpError::Transport(TransportError::Http(e.to_string())))?;

            let resp: JsonRpcResponse =
                serde_json::from_str(&body).map_err(|e| McpError::Protocol(e.to_string()))?;

            if resp.id != req.id {
                return Err(McpError::Protocol(format!(
                    "Response ID mismatch: expected {}, got {}",
                    req.id, resp.id
                )));
            }
            Ok(resp)
        }
    }

    fn subscribe_notifications(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<JsonRpcNotification>> {
        // HTTP 是无状态的，服务器无法主动推送 notification。
        // 即使 SSE 响应中解析到了 notification，那也是 request-response 管道内的附带物，
        // 不是 server → client 的主动推送通道。
        None
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            notifications: false,
        }
    }

    async fn close(&mut self) -> Result<(), McpError> {
        self.inner = None;
        self.state.send(ConnectionState::Closed).ok();
        Ok(())
    }

    fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.state.subscribe()
    }
}

/// 从请求参数中提取 Mcp-Name header 值。
///
/// 规范约定：
/// - `tools/call` → `params.name`
/// - `resources/read`, `prompts/get` → `params.uri`
fn extract_mcp_name(method: &str, params: &Option<serde_json::Value>) -> Option<String> {
    let params = params.as_ref()?;
    match method {
        "tools/call" => params
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from),
        "resources/read" | "prompts/get" => {
            params.get("uri").and_then(|v| v.as_str()).map(String::from)
        }
        _ => None,
    }
}
