//! 共享 Provider 初始化 — 所有 example 共用。
//!
//! 环境变量（按优先级）：
//! - `PROVIDER_TYPE` — 选择 provider，默认 `openai`（支持 openai / anthropic）
//! - `PROVIDER_BASE_URL` — API 基础地址，默认按 provider 类型填充
//! - `PROVIDER_API_KEY` — API Key，默认从 `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` 回退
//! - `PROVIDER_MODEL` — 模型名称，默认按 provider 类型填充
//! - `PROVIDER_TIMEOUT` — 超时秒数，默认 120

#[allow(dead_code)]
use lellm_provider::providers::anthropic::AnthropicAdapter;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;

const OPENAI_MODEL_DEFAULT: &str = "gpt-5.4";
const ANTHROPIC_MODEL_DEFAULT: &str = "claude-opus-4.6";

/// 从环境变量创建 OpenAI 兼容 Provider
pub fn create_openai_provider() -> GenericProvider<OpenAICompatAdapter> {
    let base_url = format!("{}/v1", std::env::var("OPENAI_BASE_URL").unwrap());
    GenericProvider::new(
        OpenAICompatAdapter::openai(),
        ProviderConfig {
            base_url,
            api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
            model: std::env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| OPENAI_MODEL_DEFAULT.to_string()),
            timeout_secs: std::env::var("OPENAI_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
        },
    )
}

/// 从环境变量创建 Anthropic Provider
pub fn create_anthropic_provider() -> GenericProvider<AnthropicAdapter> {
    GenericProvider::new(
        AnthropicAdapter,
        ProviderConfig {
            base_url: std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".into()),
            api_key: std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
            model: std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_else(|_| ANTHROPIC_MODEL_DEFAULT.into()),
            timeout_secs: std::env::var("ANTHROPIC_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
        },
    )
}

// /// 根据 `PROVIDER_TYPE` 环境变量创建对应 Provider 的 config。
// ///
// /// 返回 `(base_url, api_key, model, timeout_secs)` 供需要自定义 adapter 的场景使用。
// pub fn provider_config() -> (String, String, String, u64) {
//     let ptype = std::env::var("PROVIDER_TYPE").unwrap_or_else(|_| "openai".into());
//     let timeout = std::env::var("PROVIDER_TIMEOUT")
//         .ok()
//         .and_then(|s| s.parse().ok())
//         .unwrap_or(120);

//     let base_url = std::env::var("PROVIDER_BASE_URL").unwrap_or_else(|_| match ptype.as_str() {
//         "anthropic" => "https://api.anthropic.com".into(),
//         _ => "https://api.openai.com/v1".into(),
//     });

//     let api_key = std::env::var("PROVIDER_API_KEY").unwrap_or_else(|_| match ptype.as_str() {
//         "anthropic" => std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
//         _ => std::env::var("OPENAI_API_KEY").unwrap_or_default(),
//     });

//     let model = std::env::var("PROVIDER_MODEL").unwrap_or_else(|_| match ptype.as_str() {
//         "anthropic" => ANTHROPIC_MODEL_DEFAULT.into(),
//         _ => OPENAI_MODEL_DEFAULT.into(),
//     });

//     (base_url, api_key, model, timeout)
// }
