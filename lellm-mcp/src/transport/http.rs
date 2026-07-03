//! HTTP Transport — 通过 Streamable HTTP 通信。
//!
//! 架构：
//! - connect() 验证连接
//! - request() 通过 HTTP POST 发送 JSON-RPC 请求，等待响应
//! - 无状态通信，每次请求独立
//!
//! 参考：https://modelcontextprotocol.io/specification/2025-03-26/basic/transports#streamable-http

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;

use super::{ConnectionState, McpTransport, NotificationStream};
use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError};

/// 通知 channel 容量。
const NOTIFICATION_BUFFER: usize = 64;

/// 默认请求超时（秒）。
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// HTTP Transport 配置。
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// HTTP 端点 URL（如 https://mcp.map.baidu.com/mcp?ak=xxx）
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

    /// 设置请求超时。
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
    /// HTTP 客户端
    client: reqwest::Client,
    /// Notification 发送器（Streamable HTTP 可能在响应中包含 notification）
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
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

        // 验证连接：发送一个简单的 initialize 请求
        let init_params = crate::protocol::InitializeParams::new("2024-11-05")
            .with_client_info("lellm-mcp", env!("CARGO_PKG_VERSION"));
        let init_req = JsonRpcRequest::new(
            0,
            "initialize",
            Some(
                serde_json::to_value(&init_params)
                    .map_err(|e| McpError::Protocol(e.to_string()))?,
            ),
        );

        let json =
            serde_json::to_string(&init_req).map_err(|e| McpError::Protocol(e.to_string()))?;

        let response = client
            .post(&self.config.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(json)
            .send()
            .await
            .map_err(|e| McpError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(McpError::Network(format!("HTTP {}", response.status())));
        }

        self.inner = Some(Arc::new(HttpTransportInner {
            client,
            notification_tx,
        }));

        self.state.send(ConnectionState::Ready).ok();
        Ok(())
    }

    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let inner = self.inner.as_ref().ok_or(McpError::Disconnected)?;

        let json = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;

        let response = inner
            .client
            .post(&self.config.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(json)
            .send()
            .await
            .map_err(|e| McpError::Network(e.to_string()))?;

        if !response.status().is_success() {
            return Err(McpError::Network(format!("HTTP {}", response.status())));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if content_type.contains("text/event-stream") {
            // SSE 响应：解析事件流
            let bytes = response
                .bytes()
                .await
                .map_err(|e| McpError::Network(e.to_string()))?;
            let body = String::from_utf8_lossy(&bytes);

            // 简单解析 SSE 事件（实际应该用 eventsource-stream）
            for line in body.lines() {
                if line.starts_with("data: ") {
                    let data = &line[6..];
                    if let Ok(msg) = serde_json::from_str::<crate::protocol::JsonRpcMessage>(data) {
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
            }

            Err(McpError::Protocol("No response in SSE stream".to_string()))
        } else {
            // JSON 响应
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
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break None;
                        }
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
