//! lellm-core — 协议对象，零运行时依赖。
//!
//! 定义 LLM 交互的核心数据结构：Message, ContentBlock, ChatRequest,
//! ChatResponse, ToolCall, ToolDefinition, TokenUsage, LlmError 等。
//!
//! 本 crate 是纯粹的协议层（Protocol Crate），类似 openai-types / anthropic-types
//! 的统一抽象。Provider、Agent、Graph 都依赖于此，但它不依赖任何运行时。

pub mod error;
pub mod message;
pub mod prompt;
pub mod request;
pub mod response;
pub mod tool;

pub use error::{
    IntoToolError, IntoToolResult, LellmError, LlmError, MemoryError, ParseError, ToolError,
    ToolErrorKind, ToolResult,
};
pub use message::{
    CacheControl, ContentBlock, ImageSource, Message, TextBlock, ThinkingBlock, ToolCall,
    text_block,
};
pub use prompt::Prompt;
pub use request::{ChatRequest, ReasoningConfig, ToolChoice};
pub use response::{ChatResponse, TokenUsage};
#[allow(deprecated)]
pub use tool::ToolRegistration;
pub use tool::{
    __tool_box, ExecutableTool, ParallelSafety, ToolArgParser, ToolArgs, ToolCategory,
    ToolDefinition, ToolFn,
};

// Re-export for macro-generated code — ensures consistent serde/serde_json instances.
pub use schemars;
pub use serde;
pub use serde_json;
