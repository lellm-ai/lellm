//! lellm-provider — LLM Provider trait + 适配器。
//!
//! 定义统一的 `LlmProvider` trait，提供 OpenAI、Anthropic 等 provider 的
//! 具体实现。通过 feature gate 可选编译。

use std::pin::Pin;

use async_trait::async_trait;
use futures_core::Stream;
use lellm_core::{ChatRequest, ChatResponse, LlmError, TokenUsage, ToolCall};

pub mod providers;
pub mod router;

#[cfg(feature = "mock")]
pub use providers::mock::*;
pub use providers::{anthropic::*, base::*, openai_compat::*};
pub use router::{ModelRouter, ModelRouterConfig, ProviderModels, TaskLevel};

/// 流式调用返回的 Stream 类型别名。
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, LlmError>> + Send>>;

/// Provider 层流式事件
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    /// LLM 开始调用
    Start { model: String },
    /// LLM 增量令牌
    Token { token: String },
    /// LLM 调用完成
    Done {
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
}

/// 统一的 LLM Provider 接口。
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 非流式调用
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;

    /// 流式调用，返回标准 Stream。
    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError>;

    /// Provider 标识
    fn provider_id(&self) -> &str;
}
