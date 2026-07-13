//! Transport 抽象 — MCP 传输层。
//!
//! 核心设计：
//! - request() 封装 request-id 生成与匹配
//! - subscribe_notifications() 返回 broadcast Receiver（多订阅者）
//! - 状态由 Transport 主动驱动，McpClient 订阅

#[cfg(feature = "http")]
mod http;
#[cfg(feature = "sse")]
mod sse;
mod state;
#[cfg(feature = "stdio")]
mod stdio;

#[cfg(feature = "http")]
pub use http::{HttpConfig, HttpTransport};
#[cfg(feature = "sse")]
pub use sse::{SseConfig, SseTransport};
pub use state::ConnectionState;
#[cfg(feature = "stdio")]
pub use stdio::{StdioConfig, StdioTransport};
// TransportCapabilities is defined in this module

use async_trait::async_trait;

use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError};

/// Transport 能力声明（编译时固定，不依赖连接状态）。
///
/// 用于让上层（如 Registry）在不猜测 `subscribe_notifications()` 返回值的情况下，
/// 判断 Transport 是否支持某项能力。
///
/// 未来可扩展 `streaming`、`batching` 等字段。
#[derive(Debug, Clone, Default)]
pub struct TransportCapabilities {
    /// Transport 是否支持服务器主动推送 notification。
    /// - Stdio: `true`（子进程 stdout 可推送）
    /// - SSE: `true`（SSE 流可推送）
    /// - HTTP: `false`（无状态，无法主动推送）
    pub notifications: bool,
}

/// MCP Transport Trait。
///
/// 核心接口：
/// - `connect()` — 建立连接
/// - `request()` — 发送请求，等待响应（内部处理 request-id 匹配）
/// - `subscribe_notifications()` — 订阅 notification（broadcast 模型）
/// - `capabilities()` — 声明 Transport 能力（编译时固定）
/// - `close()` — 断开连接
/// - `state()` — 获取连接状态订阅
///
/// 设计理由：
/// - MCP 90% 是 request-response，notification 走独立流
/// - request-id 由 McpClient 生成，Transport 不感知
/// - 重连由 Runtime 决定，不在 Transport 层
/// - 能力声明与运行时 handle 分离（不滥用 Option 语义）
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 建立连接。
    async fn connect(&mut self) -> Result<(), McpError>;

    /// 发送 JSON-RPC Request，等待对应 Response。
    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;

    /// 订阅 notification —— broadcast 模型，多订阅者互不干扰。
    /// 返回 None 表示 Transport 不支持 notifications 或尚未 connect。
    fn subscribe_notifications(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<JsonRpcNotification>>;

    /// 声明 Transport 能力（编译时固定，不依赖连接状态）。
    fn capabilities(&self) -> TransportCapabilities;

    /// 主动断开连接。
    async fn close(&mut self) -> Result<(), McpError>;

    /// 获取连接状态订阅。
    fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState>;
}
