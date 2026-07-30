//! HTTP Transport — Streamable HTTP 传输层。
//!
//! 架构：
//! - connect() 建立连接（无状态，仅初始化 reqwest Client）
//! - request() 通过 HTTP POST 发送 JSON-RPC 请求，等待响应
//! - subscribe() 建立 subscriptions/listen 长连接 SSE 订阅
//! - 自动携带 MCP-Protocol-Version、Mcp-Method、Mcp-Name 标准 Headers
//! - 支持 application/json 与 text/event-stream 两种响应格式
//!
//! 参考：https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
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

    /// 获取内部 reqwest 客户端的克隆。
    pub fn client(&self) -> Option<reqwest::Client> {
        self.inner.as_ref().map(|inner| inner.client.clone())
    }

    /// 发送流式订阅请求，返回 SSE 长连接。
    ///
    /// **请求已由 McpClient 构建完毕**（id 分配、meta 注入）。
    /// 本方法只负责：发送 HTTP 请求 + 持续读取 SSE 流。
    ///
    /// 返回 `(receiver, handle)`：
    /// - `receiver`: broadcast channel receiver，接收 SSE 推送的 notification
    /// - `handle`: 订阅句柄，drop 时自动关闭 HTTP 连接
    pub async fn subscribe(
        &self,
        req: JsonRpcRequest,
    ) -> Result<
        (
            tokio::sync::broadcast::Receiver<JsonRpcNotification>,
            SubscriptionHandle,
        ),
        McpError,
    > {
        let inner = self.inner.as_ref().ok_or_else(McpError::disconnected)?;

        let json = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;

        // 发起 HTTP POST 请求
        let response = inner
            .client
            .post(&self.config.endpoint_url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("MCP-Protocol-Version", &self.config.protocol_version)
            .header("Mcp-Method", &req.method_name)
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

        let (notification_tx, receiver) =
            tokio::sync::broadcast::channel::<JsonRpcNotification>(NOTIFICATION_BUFFER);

        // 提取 subscription id from response header
        let sub_id = response
            .headers()
            .get("X-Subscription-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // 后台任务：持续读取 SSE 流
        let handle = spawn_subscription_reader(
            response,
            notification_tx,
            sub_id,
            self.config.endpoint_url.clone(),
        );

        Ok((receiver, handle))
    }
}

/// 订阅句柄。Drop 时自动取消 HTTP 连接。
pub struct SubscriptionHandle {
    handle: Option<tokio::task::JoinHandle<()>>,
    subscription_id: Option<String>,
}

impl SubscriptionHandle {
    /// 获取订阅 ID。
    pub fn subscription_id(&self) -> Option<&str> {
        self.subscription_id.as_deref()
    }

    /// 主动取消订阅。
    pub async fn cancel(mut self) {
        self.cancel_inner().await;
    }

    async fn cancel_inner(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _ = handle.await;
            tracing::info!(
                subscription_id = %self.subscription_id.as_deref().unwrap_or("?"),
                "Subscription cancelled"
            );
        }
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// 后台任务：持续读取 SSE 流，解析 notifications。
///
/// 使用 bytes_stream 逐 chunk 读取，缓冲不完整行以处理 chunk 边界。
fn spawn_subscription_reader(
    response: reqwest::Response,
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
    sub_id: Option<String>,
    endpoint: String,
) -> SubscriptionHandle {
    let sub_id_for_handle = sub_id.clone();
    let handle = tokio::spawn(async move {
        let log_id = sub_id.as_deref().unwrap_or("?");
        tracing::info!(
            subscription_id = %log_id,
            endpoint = %endpoint,
            "Subscription SSE reader started"
        );

        // 行缓冲：SSE 事件可能跨 chunk 边界
        let mut buffer = String::new();
        let mut event = String::new();
        let mut data = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));

                    // 处理所有完整行
                    let mut drain_until = 0;
                    for ch in buffer[drain_until..].char_indices() {
                        let abs_idx = drain_until + ch.0;
                        if ch.1 == '\n' {
                            let line = &buffer[drain_until..abs_idx];
                            if line.starts_with("event:") || line.starts_with("event: ") {
                                event = line.trim_start_matches("event:").trim().to_string();
                            } else if line.starts_with("data:") || line.starts_with("data: ") {
                                data.push_str(line.trim_start_matches("data:").trim());
                            } else if line.is_empty() && !data.is_empty() {
                                parse_and_send_sse(&event, &data, &notification_tx, log_id);
                                event.clear();
                                data.clear();
                            }
                            drain_until = abs_idx + 1;
                        }
                    }
                    if drain_until > 0 {
                        buffer.drain(..drain_until);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        subscription_id = %log_id,
                        error = %e,
                        "Subscription SSE stream error"
                    );
                    break;
                }
            }
        }

        // Flush 最后一个不完整的帧
        if !data.is_empty() {
            parse_and_send_sse(&event, &data, &notification_tx, log_id);
        }

        tracing::info!(subscription_id = %log_id, "Subscription SSE reader finished");
    });

    SubscriptionHandle {
        handle: Some(handle),
        subscription_id: sub_id_for_handle,
    }
}

/// 解析单个 SSE 事件并发送到 broadcast channel。
fn parse_and_send_sse(
    event: &str,
    data: &str,
    tx: &tokio::sync::broadcast::Sender<JsonRpcNotification>,
    sub_id: &str,
) {
    let event_type = if event.is_empty() { "message" } else { event };

    if event_type == "message" && !data.is_empty() {
        match serde_json::from_str::<crate::protocol::JsonRpcMessage>(data) {
            Ok(crate::protocol::JsonRpcMessage::Notification(ref notif)) => {
                tracing::debug!(
                    subscription_id = %sub_id,
                    method = %notif.method_name,
                    "Received subscription notification"
                );
                let _ = tx.send(notif.clone());
            }
            Ok(crate::protocol::JsonRpcMessage::Response(resp)) => {
                if let crate::protocol::JsonRpcResult::Success(v) = &resp.result {
                    tracing::info!(
                        subscription_id = %sub_id,
                        ?v,
                        "Subscription acknowledged"
                    );
                }
            }
            _ => {}
        }
    } else if event_type == "acknowledged" && !data.is_empty() {
        tracing::info!(
            subscription_id = %sub_id,
            ack_data = %data,
            "Subscription acknowledged via SSE event"
        );
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

    async fn subscribe(
        &self,
        req: JsonRpcRequest,
    ) -> Result<
        (
            tokio::sync::broadcast::Receiver<JsonRpcNotification>,
            SubscriptionHandle,
        ),
        McpError,
    > {
        Self::subscribe(self, req).await
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
