//! MCP 错误类型。

use std::io;

use thiserror::Error;

/// MCP 协议错误。
#[derive(Debug, Error)]
pub enum McpError {
    /// 连接已断开（子进程退出、网络断开）
    #[error("connection disconnected")]
    Disconnected,

    /// 请求超时
    #[error("request timeout")]
    Timeout,

    /// JSON-RPC 协议错误（格式不对、版本不匹配）
    #[error("protocol error: {0}")]
    Protocol(String),

    /// 参数无效（Server 返回 -32602）
    #[error("invalid params: {0}")]
    InvalidParams(String),

    /// Server 内部错误（Server 返回 -32603）
    #[error("server error: {0}")]
    ServerError(String),

    /// 方法未找到（Server 返回 -32601）
    #[error("method not found: {0}")]
    MethodNotFound(String),

    /// IO 错误（子进程启动失败、管道断裂）
    #[error(transparent)]
    Io(#[from] io::Error),

    /// 网络错误（HTTP 请求失败等）
    #[error("network error: {0}")]
    Network(String),
}

impl McpError {
    /// 是否值得重试。
    pub fn is_retriable(&self) -> bool {
        matches!(self, McpError::Disconnected | McpError::Timeout)
    }
}
