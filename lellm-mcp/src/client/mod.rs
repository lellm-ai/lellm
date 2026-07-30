//! MCP Client — 连接管理 + 协议层。
//!
//! 核心职责：
//! - 统一的 request id 生成（AtomicU64，单调递增，重连不重置）
//! - request<R>(method, params) 泛型入口——调用方不接触 JsonRpcRequest
//! - broadcast notification 订阅
//! - 原子恢复能力（reconnect_once，无策略）

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::protocol::{
    CallToolParams, CallToolResult, DiscoverParams, DiscoveryResult, ImplementationInfo,
    InitializeParams, InitializeResult, JsonRpcNotification, JsonRpcRequest, ListToolsResult,
    McpError, ServerError, TransportError, methods,
};
use super::transport::{ConnectionState, McpTransport, TransportCapabilities};

/// 默认协议版本（MCP 2026-07-28）。
const DEFAULT_PROTOCOL_VERSION: &str = "2026-07-28";

/// MCP Client。
///
/// 管理连接生命周期，提供统一的 request 接口。
/// 不管理重连策略（由 Runtime 决定）。
///
/// 支持 MCP 2026-07-28 无状态模式：
/// - 每请求自动注入 `_meta`（protocolVersion, clientInfo, clientCapabilities）
/// - 可选 initialize 握手（兼容旧服务器）
pub struct McpClient {
    transport: Box<dyn McpTransport>,
    /// 单调递增请求 ID，重连不重置。
    next_request_id: AtomicU64,
    /// 协议版本，用于 `_meta` 注入。
    protocol_version: Mutex<String>,
    /// 客户端身份信息，用于 `_meta` 注入。
    client_info: Mutex<Option<ImplementationInfo>>,
    /// 客户端能力，用于 `_meta` 注入。
    client_capabilities: Mutex<serde_json::Value>,
}

impl McpClient {
    /// 通过给定 Transport 创建 Client。
    pub fn with_transport<T>(transport: T) -> Self
    where
        T: McpTransport + 'static,
    {
        Self {
            transport: Box::new(transport),
            next_request_id: AtomicU64::new(1),
            protocol_version: Mutex::new(DEFAULT_PROTOCOL_VERSION.to_string()),
            client_info: Mutex::new(None),
            client_capabilities: Mutex::new(serde_json::json!({})),
        }
    }

    /// 设置客户端身份信息。
    pub async fn set_client_info(&self, name: impl Into<String>, version: impl Into<String>) {
        *self.client_info.lock().await = Some(ImplementationInfo {
            name: name.into(),
            version: version.into(),
        });
    }

    /// 设置客户端能力。
    pub async fn set_client_capabilities(&self, capabilities: serde_json::Value) {
        *self.client_capabilities.lock().await = capabilities;
    }

    /// 连接到 MCP Server。
    pub async fn connect(&mut self) -> Result<(), McpError> {
        self.transport.connect().await
    }

    /// 单次重连（connect + initialize），由 Runtime 决定是否调用。
    pub async fn reconnect_once(&mut self) -> Result<(), McpError> {
        self.transport.close().await.ok();
        self.transport.connect().await?;
        self.initialize().await.map(|_| ())
    }

    /// 发送 initialize 请求，协商协议版本。
    ///
    /// **注意**：MCP 2026-07-28 删除了 initialize 握手。
    /// 此方法仅用于兼容旧版服务器（2024-11-05, 2025-03-26, 2025-11-25）。
    /// 对于新版服务器，建议使用 `discover()`。
    pub async fn initialize(&self) -> Result<InitializeResult, McpError> {
        let version = self.protocol_version.lock().await.clone();
        let params =
            InitializeParams::new(version).with_client_info("lellm-mcp", env!("CARGO_PKG_VERSION"));
        let req = self
            .build_request(methods::INITIALIZE, Some(&params), false)
            .await?;
        self.send_and_parse(req).await
    }

    /// 发现服务器能力（MCP 2026-07-28+）。
    ///
    /// 用于替代 initialize 握手，获取服务器支持的协议版本、能力和身份信息。
    /// 可选指定客户端支持的协议版本列表。
    pub async fn discover(
        &self,
        versions: Option<Vec<String>>,
    ) -> Result<DiscoveryResult, McpError> {
        let params = match versions {
            Some(v) => DiscoverParams::new().with_versions(v),
            None => DiscoverParams::new(),
        };
        let req = self
            .build_request(methods::SERVER_DISCOVER, Some(&params), true)
            .await?;
        self.send_and_parse(req).await
    }

    /// 订阅服务器变更通知（MCP 2026-07-28+）。
    ///
    /// 建立 SSE 长连接，服务器通过该连接持续推送订阅的通知。
    /// 请求走统一流水线：状态检查 → id 分配 → meta 注入 → 发送。
    ///
    /// 返回 `(receiver, handle)`：
    /// - `receiver`: broadcast channel receiver，接收服务器推送的 notification
    /// - `handle`: 订阅句柄，drop 时自动关闭 SSE 连接
    ///
    /// 如果 Transport 不支持 subscriptions，返回 `Err(McpError::Unsupported)`。
    #[cfg(feature = "http")]
    pub async fn subscriptions_listen(
        &self,
        subscriptions: Vec<String>,
    ) -> Result<
        (
            tokio::sync::broadcast::Receiver<JsonRpcNotification>,
            super::transport::SubscriptionHandle,
        ),
        McpError,
    > {
        // 统一流水线：构建请求
        let params = super::protocol::SubscriptionsListenParams::new(subscriptions);
        let req = self
            .build_request(methods::SUBSCRIPTIONS_LISTEN, Some(&params), true)
            .await?;

        // 委托给 Transport，由 Transport 决定是否支持
        self.transport.subscribe(req).await
    }

    /// 拉取工具列表。
    pub async fn tools_list(&self) -> Result<ListToolsResult, McpError> {
        let req = self
            .build_request(methods::TOOLS_LIST, None::<&()>, true)
            .await?;
        self.send_and_parse(req).await
    }

    /// 调用工具。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolParams::new(name, arguments);
        let req = self
            .build_request(methods::TOOLS_CALL, Some(&params), true)
            .await?;
        self.send_and_parse(req).await
    }

    /// 统一的请求入口——泛型返回。
    ///
    /// 调用方只关心方法名、参数和返回类型。
    /// request id 由 McpClient 唯一生成。
    pub async fn request<P, R>(&self, method: &str, params: Option<P>) -> Result<R, McpError>
    where
        P: Serialize,
        R: for<'de> Deserialize<'de>,
    {
        let req = self.build_request(method, params.as_ref(), true).await?;
        self.send_and_parse(req).await
    }

    // ─── 统一请求流水线 ─────────────────────────────────────────────
    // 所有 outgoing request 必须经过此流水线：
    //   状态检查 → id 分配 → 序列化 → meta 注入 → transport.send()

    /// 构建一个完整的 JsonRpcRequest。
    ///
    /// 这是所有 outgoing request 的唯一入口：
    /// 1. Fail-fast 状态检查
    /// 2. 分配 request id（唯一来源）
    /// 3. 序列化 params
    /// 4. 注入 _meta（MCP 2026-07-28 无状态模式）
    async fn build_request<P>(
        &self,
        method: &str,
        params: Option<&P>,
        inject_meta: bool,
    ) -> Result<JsonRpcRequest, McpError>
    where
        P: Serialize,
    {
        // 1. Fail-fast 状态检查
        let state = *self.transport.state().borrow();
        if !state.allows_request() {
            return Err(McpError::Transport(TransportError::Disconnected));
        }

        // 2. 分配 request id（唯一来源）
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        // 3. 序列化 params
        let params_value = match params {
            Some(p) => {
                Some(serde_json::to_value(p).map_err(|e| McpError::Protocol(e.to_string()))?)
            }
            None => None,
        };

        let req = JsonRpcRequest::new(id, method, params_value);

        // 4. 注入 _meta（MCP 2026-07-28 无状态模式）
        let req = if inject_meta {
            let version = self.protocol_version.lock().await.clone();
            let client_info = self.client_info.lock().await.clone();
            let capabilities = self.client_capabilities.lock().await.clone();
            req.with_meta(version, client_info.as_ref(), Some(capabilities))
        } else {
            req
        };

        Ok(req)
    }

    /// 发送请求并解析响应。
    async fn send_and_parse<R>(&self, req: JsonRpcRequest) -> Result<R, McpError>
    where
        R: for<'de> Deserialize<'de>,
    {
        let resp = self.transport.request(req).await?;

        match resp.result {
            super::protocol::JsonRpcResult::Success(v) => {
                serde_json::from_value(v).map_err(|e| McpError::Protocol(e.to_string()))
            }
            super::protocol::JsonRpcResult::Error(e) => Err(McpError::Server(ServerError {
                code: e.code,
                message: e.message,
            })),
        }
    }

    /// 断开连接。
    pub async fn close(&mut self) -> Result<(), McpError> {
        self.transport.close().await
    }

    /// 获取当前连接状态。
    pub fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.transport.state()
    }

    /// 订阅 notification —— 委托给 Transport 的 broadcast channel。
    pub fn subscribe_notifications(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<JsonRpcNotification>> {
        self.transport.subscribe_notifications()
    }

    /// 查询 Transport 能力（编译时固定，不依赖连接状态）。
    pub fn capabilities(&self) -> TransportCapabilities {
        self.transport.capabilities()
    }

    // ─── 便捷构造 ─────────────────────────────────────────────────

    /// 创建 stdio 客户端并执行 connect + initialize。
    #[cfg(feature = "stdio")]
    pub async fn connect_stdio(
        command: impl Into<String>,
        args: Vec<String>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<Self, McpError> {
        use crate::transport::{StdioConfig, StdioTransport};
        let config = StdioConfig::new(command, args).with_env(env);
        let transport = StdioTransport::new(config);
        let mut client = Self::with_transport(transport);
        client.connect().await?;
        client.initialize().await.map(|_| client)
    }

    /// 创建 SSE 客户端并执行 connect + initialize。
    ///
    /// **已废弃**：HTTP+SSE 传输已于 MCP 2026-07-28 被标记为 Deprecated。
    /// 请使用 `connect_http`（Streamable HTTP）。
    #[cfg(feature = "sse")]
    #[deprecated(
        since = "0.5.0",
        note = "HTTP+SSE transport is deprecated per MCP 2026-07-28. Use connect_http (Streamable HTTP) instead."
    )]
    #[allow(deprecated)]
    pub async fn connect_sse(url: impl Into<String>) -> Result<Self, McpError> {
        use crate::transport::{SseConfig, SseTransport};
        let transport = SseTransport::new(SseConfig::new(url));
        let mut client = Self::with_transport(transport);
        client.connect().await?;
        client.initialize().await.map(|_| client)
    }

    /// 创建 HTTP 客户端并执行 connect + initialize。
    #[cfg(feature = "http")]
    pub async fn connect_http(url: impl Into<String>) -> Result<Self, McpError> {
        use crate::transport::{HttpConfig, HttpTransport};
        let transport = HttpTransport::new(HttpConfig::new(url));
        let mut client = Self::with_transport(transport);
        client.connect().await?;
        client.initialize().await.map(|_| client)
    }
}
