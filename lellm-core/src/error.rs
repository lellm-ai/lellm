//! 错误类型定义。

use thiserror::Error;

/// lellm 顶层错误类型。
#[derive(Debug, Error)]
pub enum LellmError {
    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),
    #[error("Tool error: {0}")]
    Tool(#[from] ToolError),
    #[error("Memory error: {0}")]
    Memory(#[from] MemoryError),
    #[error("Parse error: {0}")]
    Parse(#[from] ParseError),
}

/// LLM API 错误。
#[derive(Debug, Error)]
pub enum LlmError {
    #[error("api error: {provider} {status}")]
    ApiError {
        provider: String,
        status: u16,
        code: Option<String>,
        message: String,
    },

    #[error("request timeout")]
    Timeout,

    #[error("response parse error")]
    ParseError { detail: String },

    #[error("network error")]
    Network { detail: String },

    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    #[error("{message}")]
    Other { message: String },
}

/// 工具执行错误。
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),

    #[error("tool execution timed out")]
    Timeout,

    #[error("tool call loop detected")]
    LoopDetected,
}

/// 记忆操作错误。
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory IO error: {0}")]
    IoError(String),

    #[error("memory database error: {0}")]
    DatabaseError(String),
}

/// 解析错误。
#[derive(Debug, Error)]
#[error("parse error: {detail}")]
pub struct ParseError {
    pub detail: String,
}
