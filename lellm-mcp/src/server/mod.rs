//! MCP Server — 自定义 MCP 服务器实现。
//!
//! 参考 Python FastMCP 设计，提供简洁的 API 来定义工具并运行服务器。
//!
//! 支持两种传输方式：
//! - stdio: 本地子进程通信
//! - streamable-http: 基于 Axum 的 HTTP 服务

mod handler;
mod simple;

pub use simple::{SimpleMcp, ToolFn};

/// MCP Server 错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("invalid params: {0}")]
    InvalidParams(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
