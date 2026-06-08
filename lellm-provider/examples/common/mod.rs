//! 共享 Provider 初始化 — 所有 example 共用。
//!
//! 环境变量（按 provider 类型自动加载）：
//! - `OPENAI_BASE_URL` / `OPENAI_API_KEY` — OpenAI 兼容
//! - `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` — Anthropic
//!
//! 模型名称通过 `ChatRequest.model` 指定，不绑定在 ProviderConfig 上。

use lellm_provider::providers::anthropic::AnthropicAdapter;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;

/// 从环境变量创建 OpenAI 兼容 Provider
pub fn create_openai_provider() -> GenericProvider<OpenAICompatAdapter> {
    let adapter = OpenAICompatAdapter::openai();
    GenericProvider::new(
        adapter.clone(),
        ProviderConfig::from_adapter(&adapter).expect("OpenAI provider env error"),
    )
}

/// 从环境变量创建 Anthropic Provider
pub fn create_anthropic_provider() -> GenericProvider<AnthropicAdapter> {
    let adapter = AnthropicAdapter;
    GenericProvider::new(
        adapter.clone(),
        ProviderConfig::from_adapter(&adapter).expect("Anthropic provider env error"),
    )
}

/// 带自定义超时的 OpenAI Provider 工厂
pub fn create_openai_provider_with_timeout(
    timeout_secs: u64,
) -> GenericProvider<OpenAICompatAdapter> {
    let adapter = OpenAICompatAdapter::openai();
    GenericProvider::new(
        adapter.clone(),
        ProviderConfig::from_adapter(&adapter)
            .expect("OpenAI provider env error")
            .with_timeout(std::time::Duration::from_secs(timeout_secs)),
    )
}
