//! HTTP Transport — 通过 Streamable HTTP 通信。
//!
//! 架构：
//! - connect() 建立连接
//! - request() 通过 HTTP POST 发送 JSON-RPC 请求，等待响应
//! - 自动处理 mcp-session-id
//!
//! 参考：https://modelcontextprotocol.io/specification/2025-03-26/basic/transports#streamable-http

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, watch};

use super::{ConnectionState, McpTransport, NotificationStream};
use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError};

/// 通知 channel 容量。
const NOTIFICATION_BUFFER: usize = 64;

/// 默认请求超时（秒）。
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// HTTP Transport 配置。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// HTTP 端点 URL（如 https://mcp.map.qq.com/mcp?key=xxx&format=0）
    pub endpoint_url: String,
    /// 单次请求超时（默认 30 秒）。
    pub request_timeout: std::time::Duration,
}

impl HttpConfig {
    pub fn new(endpoint_url: impl Into<String>) -> Self {
        Self {
            endpoint_url: endpoint_url.into(),
            request_timeout: std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        }
    }

    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

/// HTTP Transport 实现。
pub struct HttpTransport {
    config: HttpConfig,
    inner: Option<Arc<HttpTransportInner>>,
    state: watch::Sender<ConnectionState>,
}

struct HttpTransportInner {
    client: reqwest::Client,
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
    /// 服务器返回的 session ID，后续请求自动携带。
    session_id: Mutex<Option<String>>,
}

impl HttpTransport {
    pub fn new(config: HttpConfig) -> Self {
        Self {
            config,
            inner: None,
            state: watch::Sender::new(ConnectionState::Disconnected),
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
            session_id: Mutex::new(None),
        }));

        self.state.send(ConnectionState::Ready).ok();
        Ok(())
    }

    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let inner = self.inner.as_ref().ok_or(McpError::Disconnected)?;

        let json = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;

        // 构建请求，自动携带 session-id
        let mut builder = inner
            .client
            .post(&self.config.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(ref sid) = *inner.session_id.lock().await {
            builder = builder.header("Mcp-Session-Id", sid);
        }

        let response = builder
            .body(json)
            .send()
            .await
            .map_err(|e| McpError::Network(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(McpError::Network(format!("HTTP {}: {}", status, body)));
        }

        // 保存 session-id
        if let Some(sid) = response.headers().get("mcp-session-id") {
            if let Ok(sid_str) = sid.to_str() {
                *inner.session_id.lock().await = Some(sid_str.to_string());
            }
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
                .map_err(|e| McpError::Network(e.to_string()))?;
            let body = String::from_utf8_lossy(&bytes);

            // SSE 格式: event:xxx\ndata:xxx\n\n (冒号后可能有空格)
            let mut current_event = String::new();
            let mut current_data = String::new();

            for line in body.lines() {
                if line.starts_with("event:") || line.starts_with("event: ") {
                    current_event = line.trim_start_matches("event:").trim().to_string();
                } else if line.starts_with("data:") || line.starts_with("data: ") {
                    current_data = line.trim_start_matches("data:").trim().to_string();
                } else if line.is_empty() && !current_data.is_empty() {
                    match current_event.as_str() {
                        "message" => {
                            if let Ok(msg) = serde_json::from_str::<crate::protocol::JsonRpcMessage>(
                                &current_data,
                            ) {
                                match msg {
                                    crate::protocol::JsonRpcMessage::Response(resp) => {
                                        return Ok(resp);
                                    }
                                    crate::protocol::JsonRpcMessage::Notification(notif) => {
                                        let _ = inner.notification_tx.send(notif);
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                    current_event.clear();
                    current_data.clear();
                }
            }

            Err(McpError::Protocol("No response in SSE stream".to_string()))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| McpError::Network(e.to_string()))?;

            let resp: JsonRpcResponse =
                serde_json::from_str(&body).map_err(|e| McpError::Protocol(e.to_string()))?;

            Ok(resp)
        }
    }

    fn notifications(&self) -> NotificationStream {
        if let Some(inner) = &self.inner {
            let rx = inner.notification_tx.subscribe();
            Box::pin(futures_util::stream::unfold(rx, move |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(notif) => break Some((notif, rx)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break None,
                    }
                }
            }))
        } else {
            Box::pin(futures_util::stream::empty())
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
