//! 共享 Provider 初始化 — 所有 example 共用。
//!
//! 环境变量（按优先级）：
//! - `PROVIDER_TYPE` — 选择 provider，默认 `openai`（支持 openai / anthropic）
//! - `PROVIDER_BASE_URL` — API 基础地址，默认按 provider 类型填充
//! - `PROVIDER_API_KEY` — API Key，默认从 `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` 回退
//! - `PROVIDER_MODEL` — 模型名称，默认按 provider 类型填充
//! - `PROVIDER_TIMEOUT` — 超时秒数，默认 120

use lellm_provider::providers::anthropic::AnthropicAdapter;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;

const OPENAI_MODEL_DEFAULT: &str = "gpt-5.4";
const ANTHROPIC_MODEL_DEFAULT: &str = "claude-opus-4.6";

/// 从环境变量创建 OpenAI 兼容 Provider
pub fn create_openai_provider() -> GenericProvider<OpenAICompatAdapter> {
    let base_url = std::env::var("OPENAI_BASE_URL").unwrap();
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| OPENAI_MODEL_DEFAULT.to_string());
    let timeout = std::env::var("OPENAI_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    GenericProvider::new(
        OpenAICompatAdapter::openai(),
        ProviderConfig::bearer(&base_url, api_key, model)
            .expect("Invalid base URL")
            .with_timeout(std::time::Duration::from_secs(timeout)),
    )
}

/// 从环境变量创建 Anthropic Provider
pub fn create_anthropic_provider() -> GenericProvider<AnthropicAdapter> {
    let base_url =
        std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into());
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| ANTHROPIC_MODEL_DEFAULT.to_string());
    let timeout = std::env::var("ANTHROPIC_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    GenericProvider::new(
        AnthropicAdapter,
        ProviderConfig::header(&base_url, "x-api-key", api_key, model)
            .expect("Invalid base URL")
            .with_timeout(std::time::Duration::from_secs(timeout)),
    )
}
