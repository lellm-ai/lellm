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
/// - Tool 执行 → `Result<T, ToolError>`
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
///
/// 错误分类：
/// - **InvalidRequest** — 调用方构造了非法请求（发请求前本地可发现）
/// - **UnsupportedFeature** — SDK 不支持的功能（能力边界）
/// - **DuplicateSystemPrompt** — 系统提示冲突
/// - **Provider** — 请求已发出，对端返回错误（401/429/500/…）
/// - **Parse** — 响应体 JSON 解析失败
/// - **Network** — 网络层错误
/// - **Timeout** — 请求超时
/// - **UnexpectedEof** — 流式输出意外结束
#[derive(Debug, Error, Clone)]
pub enum LlmError {
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    #[error("unsupported feature: {feature}")]
    UnsupportedFeature { feature: String },

    #[error("duplicate system prompt: both config and conversation contain system message")]
    DuplicateSystemPrompt,

    #[error("network error: {detail}")]
    Network { detail: String },

    #[error("request timeout: {detail}")]
    Timeout { detail: String },

    #[error("provider error [{provider}]: {message}")]
    Provider {
        provider: String,
        status: Option<u16>,
        code: Option<String>,
        message: String,
    },

    #[error("response parse error: {detail}")]
    Parse { detail: String },

    #[error("unexpected EOF: stream ended without ResponseComplete")]
    UnexpectedEof,
}

/// 工具执行错误的分类。
///
/// `Copy` 约束保留——所有变体均为 `Copy` 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// 工具未找到（静态目录中从未存在）
    NotFound,
    /// 工具不可用（动态目录中曾存在但当前刷新后消失）
    ToolUnavailable,
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
    /// 外部业务错误（由用户代码抛出，自动桥接）
    ///
    /// `source` 为原始错误类型的 `type_name`，用于可观测性。
    External { source: &'static str },
}

impl ToolErrorKind {
    /// 判断该错误是否属于基础设施层面的瞬态故障（Transient Failure）。
    ///
    /// **可重试（原地静默重试）：**
    /// - `Timeout` / `Network` / `RateLimited` — 网络抖动、服务端过载
    /// - `ToolUnavailable` — 动态目录瞬态不可用（MCP 重启等）
    ///
    /// **不可重试（立即弹回 LLM 修复层）：**
    /// - `InvalidInput` — 参数错了就是错了
    /// - `NotFound` — 工具不存在，重试也没用
    /// - `PermissionDenied` — 权限不会自动恢复
    /// - `External` — 用户业务错误，框架不应猜测
    /// - `LoopDetected` — 循环检测，重试无意义
    /// - `Internal` — 内部错误，重试无意义
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::Network | Self::RateLimited | Self::ToolUnavailable
        )
    }
}

impl fmt::Display for ToolErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "NotFound"),
            Self::ToolUnavailable => write!(f, "ToolUnavailable"),
            Self::Timeout => write!(f, "Timeout"),
            Self::Network => write!(f, "Network"),
            Self::PermissionDenied => write!(f, "PermissionDenied"),
            Self::InvalidInput => write!(f, "InvalidInput"),
            Self::RateLimited => write!(f, "RateLimited"),
            Self::LoopDetected => write!(f, "LoopDetected"),
            Self::Internal => write!(f, "Internal"),
            Self::External { source } => write!(f, "External({})", source),
        }
    }
}

/// 工具执行错误 — 携带错误分类与详细描述。
#[derive(Clone)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl ToolError {
    /// 构造 `InvalidInput` 错误。
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::InvalidInput,
            message: msg.into(),
        }
    }

    /// 构造 `NotFound` 错误。
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            kind: ToolErrorKind::NotFound,
            message: msg.into(),
        }
    }

    /// 构造 `External` 错误，自动记录原始错误类型名。
    pub fn external<E: std::fmt::Display>(source: E) -> Self {
        Self {
            kind: ToolErrorKind::External {
                source: std::any::type_name::<E>(),
            },
            message: source.to_string(),
        }
    }
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

/// 工具执行结果 — `serde_json::Value` 支持结构化数据。
///
/// 通过 `IntoToolResult` trait，用户可返回 `String`、`Value`、
/// `Option<T>`、`Result<T, E>` 等类型，框架自动转换。
pub type ToolResult = Result<serde_json::Value, ToolError>;

// ─── IntoToolError ───────────────────────────────────────────────

/// 将已知错误类型转换为 `ToolError`。
///
/// **设计原则：** 不使用 blanket impl（`impl<E: Display>`），避免吞掉 `ToolError` 原始分类。
/// 只为核心错误类型提供显式实现。
///
/// **已有实现：**
/// - `ToolError` → 直接透传
/// - `std::io::Error` → `External`
/// - `serde_json::Error` → `Internal`
/// - `anyhow::Error` → `External`（需 `anyhow` feature）
pub trait IntoToolError {
    fn into_tool_error(self) -> ToolError;
}

/// `ToolError` → 直接透传，不包装
impl IntoToolError for ToolError {
    fn into_tool_error(self) -> ToolError {
        self
    }
}

/// `std::io::Error` → `External`
impl IntoToolError for std::io::Error {
    fn into_tool_error(self) -> ToolError {
        ToolError::external(self)
    }
}

/// `serde_json::Error` → `Internal`
impl IntoToolError for serde_json::Error {
    fn into_tool_error(self) -> ToolError {
        ToolError {
            kind: ToolErrorKind::Internal,
            message: self.to_string(),
        }
    }
}

/// `anyhow::Error` → `External`
#[cfg(feature = "anyhow")]
impl IntoToolError for anyhow::Error {
    fn into_tool_error(self) -> ToolError {
        ToolError::external(self)
    }
}

// ─── IntoToolResult ──────────────────────────────────────────────

/// 将工具函数返回值统一转换为 `ToolResult`。
///
/// 由 `#[tool]` 宏在闭包中调用，用户无需手动实现。
///
/// **支持的返回类型：**
/// - `String` → `Ok(Value::String(s))`
/// - `serde_json::Value` → `Ok(v)`
/// - `T: Serialize` → `Ok(serde_json::to_value(t)?)`
/// - `Option<T>` → `Some` 转 Value，`None` → `Ok(Value::Null)`
/// - `Result<T, ToolError>` → 直接透传
/// - `Result<T, E: Display>` → `Ok` 转 Value，`Err` → `External`
pub trait IntoToolResult: Sized {
    fn into_tool(self) -> ToolResult;
}

/// `String` → `Ok(Value::String(s))`
impl IntoToolResult for String {
    fn into_tool(self) -> ToolResult {
        Ok(serde_json::Value::String(self))
    }
}

/// `serde_json::Value` → 直接透传
impl IntoToolResult for serde_json::Value {
    fn into_tool(self) -> ToolResult {
        Ok(self)
    }
}

/// `Option<T>` → `Some` 序列化，`None` → `Value::Null`
impl<T> IntoToolResult for Option<T>
where
    T: serde::Serialize,
{
    fn into_tool(self) -> ToolResult {
        match self {
            Some(v) => serde_json::to_value(v).map_err(|e| ToolError {
                kind: ToolErrorKind::Internal,
                message: format!("failed to serialize tool result: {}", e),
            }),
            None => Ok(serde_json::Value::Null),
        }
    }
}

/// `Result<T, E>` (T: Serialize, E: IntoToolError) → 自动桥接
///
/// `E: IntoToolError` 约束确保只有显式实现的错误类型才能转换。
/// `ToolError` → 直接透传，`std::io::Error` → External，`serde_json::Error` → Internal。
impl<T, E> IntoToolResult for Result<T, E>
where
    T: serde::Serialize,
    E: IntoToolError,
{
    fn into_tool(self) -> ToolResult {
        match self {
            Ok(v) => serde_json::to_value(v).map_err(|e| ToolError {
                kind: ToolErrorKind::Internal,
                message: format!("failed to serialize tool result: {}", e),
            }),
            Err(e) => Err(e.into_tool_error()),
        }
    }
}

// ─── 其他错误类型 ────────────────────────────────────────────────

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
    fn test_llm_error_provider_display() {
        let err = LlmError::Provider {
            provider: "openai".into(),
            status: Some(429),
            code: Some("rate_limit".into()),
            message: "Too many requests".into(),
        };
        assert!(format!("{}", err).contains("openai"));
        assert!(format!("{}", err).contains("Too many requests"));
    }

    #[test]
    fn test_llm_error_invalid_request_display() {
        let err = LlmError::InvalidRequest {
            message: "Anthropic requires max_tokens".into(),
        };
        assert!(format!("{}", err).contains("invalid request"));
        assert!(format!("{}", err).contains("max_tokens"));
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
    fn test_tool_error_is_retryable() {
        // 可重试
        assert!(ToolErrorKind::Timeout.is_retryable());
        assert!(ToolErrorKind::Network.is_retryable());
        assert!(ToolErrorKind::RateLimited.is_retryable());
        assert!(ToolErrorKind::ToolUnavailable.is_retryable());

        // 不可重试
        assert!(!ToolErrorKind::NotFound.is_retryable());
        assert!(!ToolErrorKind::InvalidInput.is_retryable());
        assert!(!ToolErrorKind::PermissionDenied.is_retryable());
        assert!(!ToolErrorKind::Internal.is_retryable());
        assert!(!ToolErrorKind::LoopDetected.is_retryable());
        assert!(!ToolErrorKind::External { source: "test" }.is_retryable());
    }

    #[test]
    fn test_into_tool_result_string() {
        let result: ToolResult = "hello".to_string().into_tool();
        assert_eq!(result.unwrap(), serde_json::json!("hello"));
    }

    #[test]
    fn test_into_tool_result_option() {
        let some: Option<String> = Some("hello".to_string());
        assert_eq!(some.into_tool().unwrap(), serde_json::json!("hello"));

        let none: Option<String> = None;
        assert_eq!(none.into_tool().unwrap(), serde_json::json!(null));
    }

    #[test]
    fn test_into_tool_result_external_error() {
        #[derive(Debug)]
        struct MyError;
        impl fmt::Display for MyError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "my error")
            }
        }
        // 自定义错误需显式实现 IntoToolError
        impl IntoToolError for MyError {
            fn into_tool_error(self) -> ToolError {
                ToolError::external(self)
            }
        }

        let result: ToolResult = Err::<(), MyError>(MyError).into_tool();
        let err = result.unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::External { source: std::any::type_name::<MyError>() });
        assert_eq!(err.message, "my error");
    }

    #[test]
    fn test_into_tool_result_tool_error_passthrough() {
        // ToolError 应直接透传，不被包装成 External
        let err = ToolError::invalid_input("bad param");
        let result: ToolResult = Err::<serde_json::Value, ToolError>(err).into_tool();
        let out_err = result.unwrap_err();
        assert_eq!(out_err.kind, ToolErrorKind::InvalidInput);
        assert_eq!(out_err.message, "bad param");
    }

    #[test]
    fn test_tool_error_factories() {
        let err = ToolError::invalid_input("bad input");
        assert_eq!(err.kind, ToolErrorKind::InvalidInput);
        assert_eq!(err.message, "bad input");

        let err = ToolError::not_found("search");
        assert_eq!(err.kind, ToolErrorKind::NotFound);
        assert_eq!(err.message, "search");
    }
}
