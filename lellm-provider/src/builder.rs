//! ProviderBuilder — 构建器模式创建 Provider。

use super::providers::base::ProviderConfig;

/// Provider 构建器。
///
/// # 示例
/// ```ignore
/// let provider = ProviderBuilder::new()
///     .openai(ProviderConfig {
///         base_url: "https://api.openai.com".into(),
///         api_key: "sk-...".into(),
///         model: "gpt-4o".into(),
///         ..Default::default()
///     })
///     .build();
/// ```
pub struct ProviderBuilder {
    // TODO: 存储各 provider 配置
}

impl ProviderBuilder {
    pub fn new() -> Self {
        Self {}
    }

    #[cfg(feature = "openai")]
    pub fn openai(self, _config: ProviderConfig) -> Self {
        // TODO: 存储 OpenAI 配置
        self
    }

    #[cfg(feature = "anthropic")]
    pub fn anthropic(self, _config: ProviderConfig) -> Self {
        // TODO: 存储 Anthropic 配置
        self
    }

    pub fn build(self) -> Box<dyn crate::LlmProvider> {
        // TODO: 构建 ModelRouter 或默认 provider
        unimplemented!("ProviderBuilder::build not yet implemented")
    }
}

impl Default for ProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}
