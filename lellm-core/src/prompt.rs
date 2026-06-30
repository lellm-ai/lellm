//! 分层 Prompt — 统一 System Prompt 表示，最大化前缀缓存命中率。
//!
//! # 设计目标
//!
//! - **统一概念**：`Prompt` 是唯一类型，无论简单文本还是分层结构
//! - **零迁移成本**：`impl From<String>` 让旧代码无缝升级
//! - **前缀缓存**：每层独立 `cache_control`，稳定性高的层永远命中
//!
//! # 缓存层级（稳定性递减）
//!
//! | 层级 | 内容 | 变化频率 | 缓存收益 |
//! |------|------|---------|---------|
//! | L1 | 核心身份 | 永不 | 最高 |
//! | L2 | 工具指南 | 极少 | 高 |
//! | L3 | 项目规则 | 偶尔 | 中 |
//! | L4 | 注入记忆 | 每轮 | 低 |
//! | L5 | 会话上下文 | 频繁 | 最低（通常不缓存）|

use crate::{CacheControl, ContentBlock};

/// Prompt 层 — 一段文本 + 可选的缓存控制标记。
///
/// 内部使用，不对外暴露。用户通过 `PromptBuilder` 操作。
#[derive(Debug, Clone)]
struct PromptLayer {
    text: String,
    cache_control: Option<CacheControl>,
}

/// 统一的 Prompt 表示。
///
/// 内部始终为分层结构，即使是简单文本也会转换为单层。
/// 这保证了 API 的一致性——无论用户传入 `&str` 还是 `PromptBuilder` 的结果，
/// 框架内部处理路径完全相同。
///
/// # 示例
///
/// ```
/// use lellm_core::{Prompt, PromptBuilder, CacheControl};
///
/// // 简单文本 — 自动转换
/// let simple: Prompt = "You are a helpful assistant.".into();
///
/// // 分层构建 — 最大化前缀缓存
/// let layered = Prompt::builder()
///     .layer_cached("核心身份…")               // 永不变化
///     .layer_cached("工具指南…")               // 极少变化
///     .layer_dynamic("会话上下文: …")          // 每轮变化
///     .build();
///
/// // 合并为纯文本（用于不支持 cache_control 的 Provider）
/// let text = layered.build_text();
/// ```
#[derive(Debug, Clone)]
pub struct Prompt {
    layers: Vec<PromptLayer>,
}

impl Prompt {
    /// 从纯文本创建 Prompt（单层，无缓存标记）。
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            layers: vec![PromptLayer {
                text: text.into(),
                cache_control: None,
            }],
        }
    }

    /// 创建分层构建器。
    pub fn builder() -> PromptBuilder {
        PromptBuilder::new()
    }

    /// 将 Prompt 转换为带 cache_control 的 `Vec<ContentBlock>`。
    ///
    /// 供框架内部构建 `Message::System` 使用。
    pub fn to_content_blocks(&self) -> Vec<ContentBlock> {
        self.layers
            .iter()
            .map(|layer| match layer.cache_control {
                Some(cache) => ContentBlock::text_with_cache(layer.text.clone(), cache),
                None => ContentBlock::text(&layer.text),
            })
            .collect()
    }

    /// 合并所有层为纯文本，层之间以 `\n\n` 分隔。
    ///
    /// 用于不支持 `cache_control` 的 Provider（如 OpenAI、Google）。
    pub fn build_text(&self) -> String {
        self.layers
            .iter()
            .map(|layer| layer.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// 是否为空（无层或所有层为空）。
    pub fn is_empty(&self) -> bool {
        self.layers.iter().all(|l| l.text.is_empty())
    }
}

impl From<String> for Prompt {
    fn from(s: String) -> Self {
        Self::plain(s)
    }
}

impl From<&str> for Prompt {
    fn from(s: &str) -> Self {
        Self::plain(s)
    }
}

impl From<&String> for Prompt {
    fn from(s: &String) -> Self {
        Self::plain(s.clone())
    }
}

impl Default for Prompt {
    fn default() -> Self {
        Self { layers: vec![] }
    }
}

/// Prompt 分层构建器。
///
/// 每一层可以独立设置缓存策略，最大化前缀缓存命中率。
///
/// # 示例
///
/// ```
/// use lellm_core::{Prompt, PromptBuilder, CacheControl};
///
/// let prompt = Prompt::builder()
///     // L1 — 核心身份，永不变化 → 永远命中缓存
///     .layer_cached("你是 DevOps Agent，专注于 CI/CD 管理。")
///     // L2 — 工具指南，极少变化 → 长期命中缓存
///     .layer_cached("可用工具: get_time, get_env, get_config")
///     // L3 — 项目规则，偶尔变化
///     .layer_cached("项目规则: 使用中文回复。")
///     // L4 — 分隔符
///     .layer_cached("---")
///     // L5 — 注入记忆，每轮变化
///     .layer_cached("相关记忆: 用户偏好 Jenkins。")
///     // L6 — 会话上下文，频繁变化 → 不缓存
///     .layer_dynamic("当前目标: 部署 ds-pkg")
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct PromptBuilder {
    layers: Vec<PromptLayer>,
}

impl PromptBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 添加带 `CacheControl::Breakpoint` 缓存断点的层。
    ///
    /// 用于稳定性高的内容（核心身份、工具指南、项目规则）。
    /// 这是最常用的方法——绝大多数层都应该缓存。
    pub fn layer_cached(mut self, text: impl Into<String>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            cache_control: Some(CacheControl::Breakpoint),
        });
        self
    }

    /// 添加不带缓存的层。
    ///
    /// 用于频繁变化的内容（会话上下文、临时注入信息）。
    pub fn layer_dynamic(mut self, text: impl Into<String>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            cache_control: None,
        });
        self
    }

    /// 添加带自定义缓存策略的层。
    ///
    /// 当前只有 `CacheControl::Breakpoint`，预留未来扩展。
    pub fn layer(mut self, text: impl Into<String>, cache: Option<CacheControl>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            cache_control: cache,
        });
        self
    }

    /// 构建为 `Prompt`。
    pub fn build(self) -> Prompt {
        Prompt {
            layers: self.layers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_from_string() {
        let prompt: Prompt = "hello".into();
        let blocks = prompt.to_content_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].as_text(), Some("hello"));
        if let ContentBlock::Text(t) = &blocks[0] {
            assert!(t.cache_control.is_none());
        }
    }

    #[test]
    fn test_prompt_from_str() {
        let s = "world";
        let prompt: Prompt = s.into();
        assert_eq!(prompt.build_text(), "world");
    }

    #[test]
    fn test_prompt_plain() {
        let prompt = Prompt::plain("plain text");
        assert_eq!(prompt.build_text(), "plain text");
        let blocks = prompt.to_content_blocks();
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            ContentBlock::Text(t) if t.cache_control.is_none()
        ));
    }

    #[test]
    fn test_prompt_builder_layered() {
        let prompt = Prompt::builder()
            .layer_cached("layer1")
            .layer_cached("layer2")
            .layer_dynamic("dynamic")
            .build();

        let blocks = prompt.to_content_blocks();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks.len(), 3);

        // Layer 1 — cached
        if let ContentBlock::Text(t) = &blocks[0] {
            assert_eq!(t.text, "layer1");
            assert!(t.cache_control.is_some());
        } else {
            panic!("expected Text block");
        }

        // Layer 2 — cached
        if let ContentBlock::Text(t) = &blocks[1] {
            assert_eq!(t.text, "layer2");
            assert!(t.cache_control.is_some());
        } else {
            panic!("expected Text block");
        }

        // Layer 3 — dynamic (no cache)
        if let ContentBlock::Text(t) = &blocks[2] {
            assert_eq!(t.text, "dynamic");
            assert!(t.cache_control.is_none());
        } else {
            panic!("expected Text block");
        }
    }

    #[test]
    fn test_build_text_joins_with_double_newline() {
        let prompt = Prompt::builder()
            .layer_cached("A")
            .layer_cached("B")
            .build();

        assert_eq!(prompt.build_text(), "A\n\nB");
    }

    #[test]
    fn test_build_text_single_layer() {
        let prompt = Prompt::builder().layer_cached("only").build();

        assert_eq!(prompt.build_text(), "only");
    }

    #[test]
    fn test_prompt_is_empty() {
        let empty = Prompt::default();
        assert!(empty.is_empty());

        let nonempty: Prompt = "x".into();
        assert!(!nonempty.is_empty());
    }

    #[test]
    fn test_prompt_layer_custom_cache() {
        let prompt = Prompt::builder()
            .layer("cached", Some(CacheControl::Breakpoint))
            .layer("no cache", None)
            .build();

        let blocks = prompt.to_content_blocks();
        if let ContentBlock::Text(t) = &blocks[0] {
            assert!(t.cache_control.is_some());
        } else {
            panic!("expected Text");
        }
        if let ContentBlock::Text(t) = &blocks[1] {
            assert!(t.cache_control.is_none());
        } else {
            panic!("expected Text");
        }
    }

    #[test]
    fn test_empty_layers_produce_empty_blocks() {
        let prompt = Prompt::builder().build();
        assert!(prompt.to_content_blocks().is_empty());
        assert_eq!(prompt.build_text(), "");
    }
}
