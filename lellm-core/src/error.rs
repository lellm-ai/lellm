//! 错误类型定义。

/// lellm 顶层错误类型。
#[derive(Debug)]
pub enum LellmError {
    Llm(LlmError),
    Tool(ToolError),
    Memory(MemoryError),
    Parse(ParseError),
}

impl std::fmt::Display for LellmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LellmError::Llm(e) => write!(f, "LLM error: {e}"),
            LellmError::Tool(e) => write!(f, "Tool error: {e}"),
            LellmError::Memory(e) => write!(f, "Memory error: {e}"),
            LellmError::Parse(e) => write!(f, "Parse error: {e}"),
        }
    }
}

impl std::error::Error for LellmError {}

/// LLM API 错误。
#[derive(Debug)]
pub enum LlmError {
    ApiError { status: u16, body: String },
    Timeout,
    ParseError { detail: String },
    NotFound { model: String },
    MissingApiKey { provider: String },
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::ApiError { status, body } => {
                let limit = body
                    .char_indices()
                    .nth(200)
                    .map(|(i, _)| i)
                    .unwrap_or(body.len());
                write!(f, "API error {}: {}", status, &body[..limit])
            }
            LlmError::Timeout => write!(f, "LLM API request timed out"),
            LlmError::ParseError { detail } => write!(f, "Failed to parse LLM response: {detail}"),
            LlmError::NotFound { model } => write!(f, "Model not found: {model}"),
            LlmError::MissingApiKey { provider } => {
                write!(f, "Missing API key for provider: {provider}")
            }
        }
    }
}

impl std::error::Error for LlmError {}

/// 工具执行错误。
#[derive(Debug)]
pub enum ToolError {
    NotFound(String),
    ExecutionFailed(String),
    Timeout,
    LoopDetected,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::NotFound(name) => write!(f, "Tool not found: {name}"),
            ToolError::ExecutionFailed(msg) => write!(f, "Tool execution failed: {msg}"),
            ToolError::Timeout => write!(f, "Tool execution timed out"),
            ToolError::LoopDetected => write!(f, "Tool call loop detected"),
        }
    }
}

impl std::error::Error for ToolError {}

/// 记忆操作错误。
#[derive(Debug)]
pub enum MemoryError {
    IoError(String),
    DatabaseError(String),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::IoError(msg) => write!(f, "Memory IO error: {msg}"),
            MemoryError::DatabaseError(msg) => write!(f, "Memory database error: {msg}"),
        }
    }
}

impl std::error::Error for MemoryError {}

/// 解析错误。
#[derive(Debug)]
pub struct ParseError {
    pub detail: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Parse error: {}", self.detail)
    }
}

impl std::error::Error for ParseError {}
