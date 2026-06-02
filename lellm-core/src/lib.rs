//! lellm-core — 协议对象，零运行时依赖。
//!
//! 定义 LLM 交互的核心数据结构：Message, ContentBlock, ChatRequest,
//! ChatResponse, ToolCall, ToolDefinition, TokenUsage, LlmError 等。
//!
//! 本 crate 是纯粹的协议层（Protocol Crate），类似 openai-types / anthropic-types
//! 的统一抽象。Provider、Agent、Graph 都依赖于此，但它不依赖任何运行时。

pub mod error;
pub mod message;
pub mod request;
pub mod response;

pub use error::{LellmError, LlmError, MemoryError, ParseError, ToolError};
pub use message::{
    ContentBlock, ImageSource, Message, TextBlock, ThinkingBlock, ToolCall, text_block,
};
pub use request::{ChatRequest, ToolChoice, ToolDefinition};
pub use response::{ChatResponse, TokenUsage};
