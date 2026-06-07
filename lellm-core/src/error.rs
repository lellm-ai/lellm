//! 错误类型定义。

use std::fmt;
use thiserror::Error;

/// lellm 顶层错误类型 — 门面层统一错误出口。
///
/// **架构归属：** `lellm` facade crate（聚合各子层错误）
/// **代码位置：** `lellm-core`（暂留，便于 `#[from]` 跨 crate 转换）
///
/// **铁律：Core 公共 API 禁止返回 `LellmError`。**
/// 各层必须返回各自的领域错误：
/// - Provider API → `Result<T, LlmError>`
/// - Tool 执行 → `Result<String, ToolError>`
/// - 记忆操作 → `Result<T, MemoryError>`
/// - 解析操作 → `Result<T, ParseError>`
///
/// **迁移计划：** 等 facade 承担业务逻辑时，移至 `lellm/src/error.rs`。
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
#[derive(Debug, Error, Clone)]
pub enum LlmError {
    #[error("api error: {provider} {status} {code:?} {message}")]
    ApiError {
        provider: String,
        status: u16,
        code: Option<String>,
        message: String,
    },

    #[error("authentication failed: {provider} {message}")]
    Authentication { provider: String, message: String },

    #[error("rate limited: {provider}")]
    RateLimited { provider: String },

    #[error("request timeout: {detail}")]
    Timeout { detail: String },

    #[error("unexpected EOF: stream ended without ResponseComplete")]
    UnexpectedEof,

    #[error("response parse error")]
    ParseError { detail: String },

    #[error("network error")]
    Network { detail: String },

    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    #[error("unsupported feature: {feature}")]
    UnsupportedFeature { feature: String },

    #[error("duplicate system prompt: both config and conversation contain system message")]
    DuplicateSystemPrompt,

    #[error("{message}")]
    Other { message: String },
}

/// 工具执行错误的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// 工具未找到
    NotFound,
    /// 工具执行超时
    Timeout,
    /// 网络相关错误
    Network,
    /// 权限不足
    PermissionDenied,
    /// 输入参数无效
    InvalidInput,
    /// 被限流
    RateLimited,
    /// 检测到循环调用
    LoopDetected,
    /// 内部错误（兜底）
    Internal,
}

impl ToolErrorKind {
    /// 该错误类型是否值得重试
    pub fn is_retriable(self) -> bool {
        matches!(self, Self::Timeout | Self::Network | Self::RateLimited)
    }
}

impl fmt::Display for ToolErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "NotFound"),
            Self::Timeout => write!(f, "Timeout"),
            Self::Network => write!(f, "Network"),
            Self::PermissionDenied => write!(f, "PermissionDenied"),
            Self::InvalidInput => write!(f, "InvalidInput"),
            Self::RateLimited => write!(f, "RateLimited"),
            Self::LoopDetected => write!(f, "LoopDetected"),
            Self::Internal => write!(f, "Internal"),
        }
    }
}

/// 工具执行错误 — 携带错误分类与详细描述。
#[derive(Clone)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind, self.message)
    }
}

impl fmt::Debug for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ToolError({}: {})", self.kind, self.message)
    }
}

impl std::error::Error for ToolError {}

/// 工具执行结果 — Rust 原生 Result，不包装枚举。
pub type ToolResult = Result<String, ToolError>;

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
        let err = LlmError::Timeout {
            detail: "timed out after 60s".into(),
        };
        assert!(format!("{}", err).contains("timeout"));
        assert!(format!("{}", err).contains("60s"));
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
        let err = ToolError {
            kind: ToolErrorKind::NotFound,
            message: "read_file".into(),
        };
        assert!(format!("{}", err).contains("read_file"));
    }

    #[test]
    fn test_lellm_error_from_tool_error() {
        let tool_err = ToolError {
            kind: ToolErrorKind::Timeout,
            message: "timeout".into(),
        };
        let top_err: LellmError = tool_err.into();
        assert!(format!("{}", top_err).contains("Tool error"));
    }

    #[test]
    fn test_tool_error_is_retriable() {
        assert!(ToolErrorKind::Timeout.is_retriable());
        assert!(ToolErrorKind::Network.is_retriable());
        assert!(ToolErrorKind::RateLimited.is_retriable());
        assert!(!ToolErrorKind::NotFound.is_retriable());
        assert!(!ToolErrorKind::InvalidInput.is_retriable());
    }
}
