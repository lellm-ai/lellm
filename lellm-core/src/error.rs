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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_error_display() {
        let err = LlmError::Timeout;
        assert_eq!(format!("{}", err), "request timeout");
    }

    #[test]
    fn test_llm_error_api_error_display() {
        let err = LlmError::ApiError {
            provider: "openai".into(),
            status: 429,
            code: Some("rate_limit".into()),
            message: "Too many requests".into(),
        };
        assert!(format!("{}", err).contains("openai"));
        assert!(format!("{}", err).contains("429"));
    }

    #[test]
    fn test_tool_error_display() {
        let err = ToolError::NotFound("read_file".into());
        assert_eq!(format!("{}", err), "tool not found: read_file");
    }

    #[test]
    fn test_lellm_error_from_llm_error() {
        let llm_err = LlmError::Timeout;
        let top_err: LellmError = llm_err.into();
        assert!(format!("{}", top_err).contains("LLM error"));
    }
}
