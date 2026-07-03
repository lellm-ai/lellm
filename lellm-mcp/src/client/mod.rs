//! MCP Client — 连接管理 + 工具发现。

use std::sync::Arc;

use super::protocol::{InitializeParams, JsonRpcRequest, JsonRpcResponse, McpError, methods};
use super::transport::{ConnectionState, McpTransport};

/// MCP Client。
///
/// 管理连接生命周期，提供 request 接口。
pub struct McpClient {
    transport: Arc<tokio::sync::Mutex<dyn McpTransport>>,
    state: tokio::sync::watch::Receiver<ConnectionState>,
}

impl McpClient {
    /// 通过给定 Transport 创建 Client（同步版本，适合在 runtime 外使用）。
    pub fn with_transport<T>(transport: T) -> Self
    where
        T: McpTransport + 'static,
    {
        let transport = Arc::new(tokio::sync::Mutex::new(transport));
        // 在同步上下文中使用 try_lock，如果失败则创建一个默认的 state receiver
        let state = match transport.try_lock() {
            Ok(t) => t.state(),
            Err(_) => {
                // 创建一个临时的 watch channel
                let (_, rx) = tokio::sync::watch::channel(ConnectionState::Disconnected);
                rx
            }
        };
        Self { transport, state }
    }

    /// 通过给定 Transport 创建 Client（异步版本，推荐使用）。
    pub async fn with_transport_async<T>(transport: T) -> Self
    where
        T: McpTransport + 'static,
    {
        let transport = Arc::new(tokio::sync::Mutex::new(transport));
        let state = transport.lock().await.state();
        Self { transport, state }
    }

    /// 连接到 MCP Server。
    pub async fn connect(&self) -> Result<(), McpError> {
        self.transport.lock().await.connect().await
    }

    /// 发送 initialize 请求。
    pub async fn initialize(&self) -> Result<crate::protocol::InitializeResult, McpError> {
        let params = InitializeParams::new("2024-11-05")
            .with_client_info("lellm-mcp", env!("CARGO_PKG_VERSION"));
        let params_value =
            serde_json::to_value(&params).map_err(|e| McpError::Protocol(e.to_string()))?;

        let req = JsonRpcRequest::new(0, methods::INITIALIZE, Some(params_value));
        let resp = self.request(req).await?;

        let result: crate::protocol::InitializeResult = serde_json::from_value(match resp.result {
            crate::protocol::JsonRpcResult::Success(v) => v,
            crate::protocol::JsonRpcResult::Error(e) => {
                return Err(McpError::ServerError(e.message));
            }
        })
        .map_err(|e| McpError::Protocol(e.to_string()))?;

        Ok(result)
    }

    /// 发送 JSON-RPC Request。
    pub async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        // Fail-fast: 非 Ready 状态直接返回
        let state = *self.state.borrow();
        if !state.allows_request() {
            return Err(McpError::Disconnected);
        }

        self.transport.lock().await.request(req).await
    }

    /// 断开连接。
    pub async fn close(&self) -> Result<(), McpError> {
        self.transport.lock().await.close().await
    }

    /// 获取当前连接状态。
    pub fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.state.clone()
    }
}
