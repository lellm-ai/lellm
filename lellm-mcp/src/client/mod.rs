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
    CallToolParams, CallToolResult, InitializeParams, InitializeResult, JsonRpcNotification,
    JsonRpcRequest, ListToolsResult, McpError, ServerError, TransportError, methods,
};
use super::transport::{ConnectionState, McpTransport};

/// MCP Client。
///
/// 管理连接生命周期，提供统一的 request 接口。
/// 不管理重连策略（由 Runtime 决定）。
pub struct McpClient {
    transport: Box<dyn McpTransport>,
    /// 单调递增请求 ID，重连不重置。
    next_request_id: AtomicU64,
    /// initialize 协商后的协议版本，后续请求自动注入。
    protocol_version: Mutex<Option<String>>,
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
            protocol_version: Mutex::new(None),
        }
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
    pub async fn initialize(&self) -> Result<InitializeResult, McpError> {
        let params = InitializeParams::new("2024-11-05")
            .with_client_info("lellm-mcp", env!("CARGO_PKG_VERSION"));
        self.request_inner(methods::INITIALIZE, Some(&params), false)
            .await
    }

    /// 拉取工具列表。
    pub async fn tools_list(&self) -> Result<ListToolsResult, McpError> {
        self.request_inner(methods::TOOLS_LIST, None::<&()>, true)
            .await
    }

    /// 调用工具。
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<CallToolResult, McpError> {
        let params = CallToolParams::new(name, arguments);
        self.request_inner(methods::TOOLS_CALL, Some(&params), true)
            .await
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
        self.request_inner(method, params.as_ref(), true).await
    }

    /// 内部请求方法。
    async fn request_inner<P, R>(
        &self,
        method: &str,
        params: Option<&P>,
        inject_protocol_version: bool,
    ) -> Result<R, McpError>
    where
        P: Serialize,
        R: for<'de> Deserialize<'de>,
    {
        // Fail-fast 检查
        let state = *self.transport.state().borrow();
        if !state.allows_request() {
            return Err(McpError::Transport(TransportError::Disconnected));
        }

        // 分配 request id
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        // 序列化 params
        let params_value = match params {
            Some(p) => {
                Some(serde_json::to_value(p).map_err(|e| McpError::Protocol(e.to_string()))?)
            }
            None => None,
        };

        // 自动注入 protocolVersion
        let params_value = if inject_protocol_version {
            if let Some(ref ver) = *self.protocol_version.lock().await {
                let mut params = params_value.unwrap_or_else(|| serde_json::json!({}));
                // 不能合并：内层需要可变引用存活足够久以完成 insert
                #[allow(clippy::collapsible_if)]
                if let Some(obj) = params.as_object_mut() {
                    if !obj.contains_key("protocolVersion") {
                        obj.insert(
                            "protocolVersion".to_string(),
                            serde_json::Value::String(ver.clone()),
                        );
                    }
                }
                Some(params)
            } else {
                params_value
            }
        } else {
            params_value
        };

        let req = JsonRpcRequest::new(id, method, params_value);
        let resp = self.transport.request(req).await?;

        // 解析结果
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
    #[cfg(feature = "sse")]
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
