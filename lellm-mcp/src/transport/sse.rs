//! SSE Transport — 通过 Server-Sent Events 接收，HTTP POST 发送。
//!
//! 架构：
//! - connect() 建立 SSE 连接，监听服务器事件
//! - request() 通过 HTTP POST 发送 JSON-RPC 请求
//! - 服务器通过 SSE 推送 Response 和 Notification
//!
//! 参考：https://modelcontextprotocol.io/specification/2025-03-26/basic/transports#sse

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;

use super::{ConnectionState, McpTransport};
use crate::protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError, TransportError,
};

/// SSE 事件类型常量。
const EVENT_ENDPOINT: &str = "endpoint";
const EVENT_MESSAGE: &str = "message";

/// 通知 channel 容量。
const NOTIFICATION_BUFFER: usize = 64;

/// 默认请求超时（秒）。
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// SSE Transport 配置。
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// SSE 端点 URL（如 https://mcp.map.baidu.com/sse?ak=xxx）
    pub sse_url: String,
    /// 单次请求超时（默认 30 秒）。
    pub request_timeout: std::time::Duration,
}

impl SseConfig {
    pub fn new(sse_url: impl Into<String>) -> Self {
        Self {
            sse_url: sse_url.into(),
            request_timeout: std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        }
    }

    /// 设置请求超时。
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

/// SSE Transport 实现。
pub struct SseTransport {
    config: SseConfig,
    inner: Option<Arc<SseTransportInner>>,
    state: watch::Sender<ConnectionState>,
    /// 持有 watch channel 的 receiver，确保 sender 始终有 subscriber，send() 才能更新值。
    #[allow(dead_code)]
    _state_rx: watch::Receiver<ConnectionState>,
}

struct SseTransportInner {
    /// HTTP POST 端点（从 SSE endpoint 事件获取，watch 替代 polling）
    post_url: watch::Sender<Option<String>>,
    /// 待处理的请求
    pending: Arc<Mutex<PendingMap>>,
    /// Notification 发送器
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
    /// HTTP 客户端
    client: reqwest::Client,
    /// SSE 连接句柄
    sse_handle: Mutex<Option<JoinHandle<()>>>,
    /// 关闭信号
    shutdown: watch::Sender<bool>,
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<JsonRpcResponse, McpError>>>;

impl SseTransport {
    pub fn new(config: SseConfig) -> Self {
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
impl McpTransport for SseTransport {
    async fn connect(&mut self) -> Result<(), McpError> {
        self.state.send(ConnectionState::Connecting).ok();

        let client = reqwest::Client::new();
        let client_clone = client.clone();
        let (notification_tx, _) =
            tokio::sync::broadcast::channel::<JsonRpcNotification>(NOTIFICATION_BUFFER);
        let (shutdown_tx, _) = watch::channel(false);
        let (post_url_tx, _) = watch::channel::<Option<String>>(None);

        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        // 启动 SSE 连接
        let sse_url = self.config.sse_url.clone();
        let pending_clone = pending.clone();
        let post_url_clone = post_url_tx.clone();
        let notification_tx_clone = notification_tx.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let state_tx = self.state.clone();

        let sse_handle = tokio::spawn(async move {
            let mut event_stream = match client_clone
                .get(&sse_url)
                .header("Accept", "text/event-stream")
                .header("Cache-Control", "no-cache")
                .header("Connection", "keep-alive")
                .send()
                .await
            {
                Ok(resp) => resp.bytes_stream().eventsource(),
                Err(e) => {
                    tracing::error!(error = %e, "SSE connection failed");
                    state_tx.send(ConnectionState::Disconnected).ok();
                    return;
                }
            };

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    event = event_stream.next() => {
                        match event {
                            Some(Ok(event)) => {
                                match event.event.as_str() {
                                    EVENT_ENDPOINT => {
                                        // 获取 POST URL（可能是相对路径，需要拼接完整 URL）
                                        let post_url_str = event.data.clone();
                                        let full_url = if post_url_str.starts_with("http") {
                                            post_url_str
                                        } else {
                                            // 从 SSE URL 提取 base URL
                                            let base_url = sse_url.rsplit_once('/').map(|(base, _)| base).unwrap_or(&sse_url);
                                            format!("{}{}", base_url, post_url_str)
                                        };
                                        // watch::send_replace — 支持 reconnect 后更新
                                        post_url_clone.send_replace(Some(full_url));
                                    }
                                    EVENT_MESSAGE => {
                                        tracing::debug!(data = %event.data, "Received message event");
                                        // 解析 JSON-RPC 消息
                                        let msg: JsonRpcMessage = match serde_json::from_str(&event.data) {
                                            Ok(msg) => msg,
                                            Err(e) => {
                                                tracing::warn!(error = %e, data = %event.data, "Invalid JSON-RPC");
                                                continue;
                                            }
                                        };

                                        match msg {
                                            JsonRpcMessage::Response(resp) => {
                                                tracing::debug!(id = resp.id, "Received response");
                                                let mut p = pending_clone.lock().await;
                                                if let Some(tx) = p.remove(&resp.id) {
                                                    let _ = tx.send(Ok(resp));
                                                } else {
                                                    tracing::warn!(id = resp.id, "No pending request for response");
                                                }
                                            }
                                            JsonRpcMessage::Notification(notif) => {
                                                tracing::debug!("Received notification");
                                                let _ = notification_tx_clone.send(notif);
                                            }
                                            JsonRpcMessage::Request(_) => {
                                                tracing::warn!("unexpected request from server");
                                            }
                                        }
                                    }
                                    _ => {
                                        tracing::debug!(event_type = %event.event, data = %event.data, "Unknown SSE event");
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                tracing::error!(error = %e, "SSE event error");
                                break;
                            }
                            None => {
                                tracing::info!("SSE stream ended");
                                break;
                            }
                        }
                    }
                }
            }

            // SSE 退出 → 更新状态 → 清理 pending
            state_tx.send(ConnectionState::Disconnected).ok();

            let pending_to_fail = {
                let mut p = pending_clone.lock().await;
                std::mem::take(&mut *p)
            };
            for (_, tx) in pending_to_fail {
                let _ = tx.send(Err(McpError::Transport(TransportError::Disconnected)));
            }
        });

        let mut post_url_rx = post_url_tx.subscribe();

        self.inner = Some(Arc::new(SseTransportInner {
            post_url: post_url_tx,
            pending,
            notification_tx,
            client,
            sse_handle: Mutex::new(Some(sse_handle)),
            shutdown: shutdown_tx,
        }));

        // 等待获取 POST URL（watch 事件驱动，零轮询）
        let timeout = std::time::Duration::from_secs(10);
        if post_url_rx.borrow().is_some() {
            // 已经收到 endpoint
        } else if let Err(_) = tokio::time::timeout(timeout, post_url_rx.changed()).await {
            self.state.send(ConnectionState::Disconnected).ok();
            return Err(McpError::Transport(TransportError::Timeout));
        }

        // 验证 post_url 确实有值（changed 可能被 send_replace(None) 触发）
        if post_url_rx.borrow().as_ref().is_none() {
            self.state.send(ConnectionState::Disconnected).ok();
            return Err(McpError::Transport(TransportError::Timeout));
        }

        self.state.send(ConnectionState::Ready).ok();
        Ok(())
    }

    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let inner = self.inner.as_ref().ok_or_else(McpError::disconnected)?;

        // 获取 POST URL
        let post_url = inner
            .post_url
            .borrow()
            .clone()
            .ok_or_else(McpError::disconnected)?;

        // 直接使用 McpClient 生成的 request id
        let id = req.id;
        let method = req.method_name.clone();

        // 注册 pending
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().await.insert(id, tx);

        // 通过 HTTP POST 发送
        let json = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;

        let response = inner
            .client
            .post(&post_url)
            .header("Content-Type", "application/json")
            .body(json)
            .send()
            .await
            .map_err(|e| McpError::Transport(TransportError::Http(e.to_string())))?;

        // 检查 HTTP 状态
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::error!(status = %status, body = %body, "POST request failed");
            inner.pending.lock().await.remove(&id);
            return Err(McpError::Transport(TransportError::Http(format!(
                "HTTP {}: {}",
                status, body
            ))));
        }

        tracing::debug!(method = %method, "Request sent successfully");

        // 等待 SSE 推送的响应（带超时）
        match tokio::time::timeout(self.config.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::Transport(TransportError::Disconnected)),
            Err(_elapsed) => {
                // 超时 — 清理 pending entry
                inner.pending.lock().await.remove(&id);
                tracing::warn!(
                    method = %method,
                    timeout_ms = self.config.request_timeout.as_millis() as u64,
                    "MCP request timed out"
                );
                Err(McpError::Transport(TransportError::Timeout))
            }
        }
    }

    fn subscribe_notifications(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<JsonRpcNotification>> {
        self.inner
            .as_ref()
            .map(|inner| inner.notification_tx.subscribe())
    }

    async fn close(&mut self) -> Result<(), McpError> {
        if let Some(ref inner) = self.inner {
            let _ = inner.shutdown.send(true);
            if let Some(handle) = inner.sse_handle.lock().await.take() {
                handle.abort();
            }
        }
        self.inner = None;
        self.state.send(ConnectionState::Closed).ok();
        Ok(())
    }

    fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.state.subscribe()
    }
}
