//! MCP Client — 连接管理 + 工具发现。

use std::sync::Arc;

use tokio::sync::Mutex;

use super::protocol::{InitializeParams, JsonRpcRequest, JsonRpcResponse, McpError, methods};
use super::transport::{ConnectionState, McpTransport};

/// MCP Client。
///
/// 管理连接生命周期，提供 request 接口。
pub struct McpClient {
    transport: Arc<Mutex<dyn McpTransport>>,
    state: tokio::sync::watch::Receiver<ConnectionState>,
    /// initialize 协商后的协议版本，后续请求自动注入。
    protocol_version: Arc<Mutex<Option<String>>>,
}

impl McpClient {
    /// 通过给定 Transport 创建 Client（异步版本）。
    pub async fn with_transport<T>(transport: T) -> Self
    where
        T: McpTransport + 'static,
    {
        let transport = Arc::new(Mutex::new(transport));
        let state = transport.lock().await.state();
        Self {
            transport,
            state,
            protocol_version: Arc::new(Mutex::new(None)),
        }
    }

    /// 连接到 MCP Server。
    pub async fn connect(&self) -> Result<(), McpError> {
        self.transport.lock().await.connect().await
    }

    /// 发送 initialize 请求，协商协议版本。
    pub async fn initialize(&self) -> Result<crate::protocol::InitializeResult, McpError> {
        let params = InitializeParams::new("2024-11-05")
            .with_client_info("lellm-mcp", env!("CARGO_PKG_VERSION"));
        let params_value =
            serde_json::to_value(&params).map_err(|e| McpError::Protocol(e.to_string()))?;

        let req = JsonRpcRequest::new(0, methods::INITIALIZE, Some(params_value));
        let resp = self.request_raw(req).await?;

        let result: crate::protocol::InitializeResult = serde_json::from_value(match resp.result {
            crate::protocol::JsonRpcResult::Success(v) => v,
            crate::protocol::JsonRpcResult::Error(e) => {
                return Err(McpError::ServerError(e.message));
            }
        })
        .map_err(|e| McpError::Protocol(e.to_string()))?;

        // 保存协议版本，后续请求自动注入
        *self.protocol_version.lock().await = Some(result.protocol_version.clone());

        Ok(result)
    }

    /// 发送 JSON-RPC Request（自动注入 protocolVersion）。
    pub async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let state = *self.state.borrow();
        if !state.allows_request() {
            return Err(McpError::Disconnected);
        }

        // 非 initialize 请求自动注入 protocolVersion
        let req = if req.method_name != methods::INITIALIZE {
            if let Some(ref ver) = *self.protocol_version.lock().await {
                let mut params = req.params.unwrap_or(serde_json::json!({}));
                if let Some(obj) = params.as_object_mut() {
                    if !obj.contains_key("protocolVersion") {
                        obj.insert(
                            "protocolVersion".to_string(),
                            serde_json::Value::String(ver.clone()),
                        );
                    }
                }
                JsonRpcRequest::new(req.id, &req.method_name, Some(params))
            } else {
                req
            }
        } else {
            req
        };

        self.transport.lock().await.request(req).await
    }

    /// 发送 JSON-RPC Request（不注入 protocolVersion，仅供 initialize 使用）。
    async fn request_raw(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
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
