//! Transport 抽象 — MCP 传输层。
//!
//! 核心设计：
//! - request() 封装 request-id 生成与匹配
//! - notifications() 返回独立流（不阻塞主流程）
//! - 状态机由 McpClient 管理，Transport 不感知

mod state;
#[cfg(feature = "stdio")]
mod stdio;

pub use state::ConnectionState;
#[cfg(feature = "stdio")]
pub use stdio::{StdioConfig, StdioTransport};

use async_trait::async_trait;

use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError};

/// Notification 流类型。
pub type NotificationStream =
    futures_util::stream::BoxStream<'static, JsonRpcNotification>;

/// MCP Transport Trait。
///
/// 核心接口：
/// - `connect()` — 建立连接
/// - `request()` — 发送请求，等待响应（内部处理 request-id 匹配）
/// - `notifications()` — 获取 notification 流（可选消费）
/// - `close()` — 断开连接
/// - `state()` — 获取连接状态订阅
///
/// 设计理由：
/// - MCP 90% 是 request-response，notification 走独立流
/// - request-id 内部化，调用者无需管理
/// - 重连由 McpClient 管理，不在 Transport 层
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// 建立连接。
    async fn connect(&mut self) -> Result<(), McpError>;

    /// 发送 JSON-RPC Request，等待对应 Response。
    ///
    /// - 内部处理 request-id 生成与匹配
    /// - 超时由调用者控制
    /// - 连接断开返回 `McpError::Disconnected`
    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;

    /// 获取 notification 流。
    ///
    /// - 调用者可选消费（不消费则静默丢弃）
    /// - 内部使用有界 channel，满时丢弃最新（背压）
    fn notifications(&self) -> NotificationStream;

    /// 主动断开连接。
    async fn close(&mut self) -> Result<(), McpError>;

    /// 获取连接状态订阅。
    fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState>;
}
