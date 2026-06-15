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
pub use providers::{anthropic::*, base::*, codec::*, google::*, openai_compat::*};
pub use router::{ModelRouter, ProviderRegistry, ResolvedModel, RouteEntry, TaskLevel};

/// 流式调用返回的 Stream 类型别名。
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, LlmError>> + Send>>;

/// Provider 层流式事件
#[derive(Debug, Clone)]
pub enum ProviderEvent {
    /// LLM 开始调用
    Start { model: String },
    /// LLM 增量令牌
    Token { token: String },
    /// LLM 思考块增量（Claude thinking / OpenAI reasoning_content）
    ThinkingDelta {
        thinking: String,
        redacted: Option<String>,
    },
    /// 单次 LLM 响应结束（HTTP/SSE 请求完成）。
    ///
    /// 注意：这不等于 Agent 推理结束。如果 `tool_calls` 非空，
    /// Agent 会继续执行工具并发起下一轮调用。
    ResponseComplete {
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

    /// 返回指定模型的能力矩阵。
    ///
    /// 默认实现返回全 false（最保守假设）。
    /// Provider 应 override 以提供精确的能力声明。
    fn capabilities_for(&self, _model: &str) -> Capabilities {
        Capabilities::default()
    }
}
