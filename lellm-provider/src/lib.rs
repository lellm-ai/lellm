//! lellm-provider — LLM Provider trait + 适配器。
//!
//! 定义统一的 `LlmProvider` trait，提供 OpenAI、Anthropic 等 provider 的
//! 具体实现。通过 feature gate 可选编译。

use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;
use lellm_core::{ChatRequest, ChatResponse, LlmError, ToolCall};

pub mod builder;
pub mod providers;
pub mod router;

pub use builder::ProviderBuilder;
#[cfg(feature = "mock")]
pub use providers::mock::*;
pub use providers::{anthropic::*, base::*, openai_compat::*};
pub use router::{ModelRouter, ModelRouterConfig, ProviderModels, TaskLevel};

/// 流式调用返回的 Stream 类型别名。
/// Provider 内部可用 mpsc::Receiver 实现，对外暴露标准 Stream trait。
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

/// 流式事件类型
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// LLM 开始调用
    LlmStart {
        model: String,
        messages_count: usize,
    },
    /// LLM 增量令牌
    LlmToken { token: String },
    /// LLM 调用完成，包含 tool_calls
    LlmEnd { tool_calls: Vec<ToolCall> },
    /// 工具开始执行
    ToolStart { tool_call_id: String, name: String },
    /// 工具执行完成
    ToolEnd {
        tool_call_id: String,
        result: String,
    },
    /// 自定义更新（用户定义的进度信号）
    Custom { data: serde_json::Value },
}

/// 流式模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// 仅代理进度（updates）
    Updates,
    /// LLM 令牌 + 元数据（messages）
    Messages,
    /// 自定义事件（custom）
    Custom,
}

/// 统一的 LLM Provider 接口。
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 非流式调用
    async fn llm_call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;

    /// 流式调用，返回标准 Stream。
    /// Provider 内部可用 mpsc::Receiver 实现，转为 BoxStream 返回。
    async fn llm_call_stream(&self, request: &ChatRequest) -> Result<LlmStream, LlmError>;

    /// Provider 标识
    fn provider_id(&self) -> &str;
}
