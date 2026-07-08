//! MCP 错误类型。
//!
//! 核心设计：
//! - 错误来源（TransportError）与恢复策略（RetryDisposition）分离
//! - McpError 不暴露 is_retriable() bool——信息太少
//! - Runtime 根据 retry_disposition() 决定恢复行为

use std::io;

use thiserror::Error;

/// 传输层错误——只描述"哪里出了问题"。
#[derive(Debug, Error)]
pub enum TransportError {
    /// 连接已断开（SSE stream 结束、子进程退出）
    #[error("connection disconnected")]
    Disconnected,

    /// 请求超时
    #[error("request timeout")]
    Timeout,

    /// DNS 解析失败
    #[error("dns resolution failed: {0}")]
    Dns(String),

    /// TCP 连接失败（refused、reset 等）
    #[error("connection failed: {0}")]
    Connect(String),

    /// TLS 握手失败
    #[error("tls handshake failed: {0}")]
    Tls(String),

    /// HTTP 传输错误
    #[error("http transport error: {0}")]
    Http(String),

    /// IO 错误（子进程启动失败、管道断裂）
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// 恢复策略分类——Runtime 根据此枚举决定行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDisposition {
    /// 不可重试（参数错误、方法不存在等）
    Never,
    /// 立即重试（同一连接重发，不需要重连）
    Immediate,
    /// 需要重连（调用 reconnect_once() 后重试）
    Reconnect,
    /// 指数退避后重试（网络抖动等）
    Backoff,
}

/// Server 返回的错误。
#[derive(Debug, Clone)]
pub struct ServerError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for ServerError {}

/// MCP 统一错误类型。
#[derive(Debug, Error)]
pub enum McpError {
    /// 传输层错误
    #[error(transparent)]
    Transport(#[from] TransportError),

    /// JSON-RPC 协议错误（格式不对、版本不匹配）
    #[error("protocol error: {0}")]
    Protocol(String),

    /// 参数无效（Server 返回 -32602）
    #[error("invalid params: {0}")]
    InvalidParams(String),

    /// Server 内部错误
    #[error(transparent)]
    Server(#[from] ServerError),

    /// 方法未找到（Server 返回 -32601）
    #[error("method not found: {0}")]
    MethodNotFound(String),
}

impl McpError {
    /// 获取恢复策略分类。
    ///
    /// Runtime 根据此值决定：
    /// - `Never` → 直接报错，不重试
    /// - `Immediate` → 同一连接重发
    /// - `Reconnect` → 调用 reconnect_once() 后重试
    /// - `Backoff` → 指数退避后重试
    pub fn retry_disposition(&self) -> RetryDisposition {
        match self {
            McpError::Transport(transport) => match transport {
                TransportError::Disconnected => RetryDisposition::Reconnect,
                TransportError::Timeout => RetryDisposition::Immediate,
                TransportError::Dns(_) => RetryDisposition::Backoff,
                TransportError::Connect(_) => RetryDisposition::Backoff,
                TransportError::Tls(_) => RetryDisposition::Never,
                TransportError::Http(_) => RetryDisposition::Backoff,
                TransportError::Io(io_err) => {
                    // 连接重置/断开发起重连，其他 IO 错误退避
                    match io_err.kind() {
                        io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::BrokenPipe => RetryDisposition::Reconnect,
                        _ => RetryDisposition::Backoff,
                    }
                }
            },
            McpError::Protocol(_) => RetryDisposition::Never,
            McpError::InvalidParams(_) => RetryDisposition::Never,
            McpError::Server(_) => RetryDisposition::Never,
            McpError::MethodNotFound(_) => RetryDisposition::Never,
        }
    }

    /// 兼容性方法——是否值得重试（返回 true = 非 Never）。
    #[deprecated(since = "0.5.0", note = "Use retry_disposition() instead")]
    pub fn is_retriable(&self) -> bool {
        !matches!(self.retry_disposition(), RetryDisposition::Never)
    }
}

// 向后兼容的别名——供现有代码使用，后续逐步迁移。
impl McpError {
    pub fn disconnected() -> Self {
        McpError::Transport(TransportError::Disconnected)
    }

    pub fn timeout() -> Self {
        McpError::Transport(TransportError::Timeout)
    }

    pub fn network(msg: impl Into<String>) -> Self {
        McpError::Transport(TransportError::Http(msg.into()))
    }
}
