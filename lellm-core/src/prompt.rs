//! 分层 Prompt — 统一 System Prompt 表示，最大化前缀缓存命中率。
//!
//! # 设计
//!
//! - **Prompt** 只产出 `Message::System`，Provider 不感知 Prompt 的存在
//! - **断点放置**：最后一个 `stable` layer 自动获得 `CacheControl::Breakpoint`
//! - **Provider 消费**：Anthropic 直接使用 `cache_control`；OpenAI/Gemini 调用 `ContentBlock::flatten_text()`
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

use crate::{CacheControl, ContentBlock, Message};

/// Prompt 层 — 一段文本 + 稳定性标记。
///
/// 内部使用，不对外暴露。用户通过 `PromptBuilder` 操作。
#[derive(Debug, Clone)]
struct PromptLayer {
    text: String,
    /// 是否属于稳定前缀（参与缓存）。
    stable: bool,
}

/// 统一的 Prompt 表示。
///
/// 内部始终为分层结构，即使是简单文本也会转换为单层。
/// `build()` 产出 `Message::System`，断点已自动放置。
///
/// # 示例
///
/// ```
/// use lellm_core::Prompt;
///
/// let msg = Prompt::builder()
///     .stable("核心身份…")
///     .stable("工具指南…")
///     .dynamic("会话上下文: …")
///     .finish()
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct Prompt {
    layers: Vec<PromptLayer>,
}

impl Prompt {
    /// 从纯文本创建 Prompt（单层，动态）。
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            layers: vec![PromptLayer {
                text: text.into(),
                stable: false,
            }],
        }
    }

    /// 创建分层构建器。
    pub fn builder() -> PromptBuilder {
        PromptBuilder::new()
    }

    /// 构建为 `Message::System`。
    ///
    /// 断点放置策略：只在最后一个 `stable` layer 上放置 `CacheControl::Breakpoint`。
    /// Anthropic 每个请求最多 4 个断点，中间断点不产生独立缓存段，纯属浪费。
    pub fn build(self) -> Message {
        let last_stable_idx = self
            .layers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, layer)| layer.stable)
            .map(|(idx, _)| idx);

        let content: Vec<ContentBlock> = self
            .layers
            .iter()
            .enumerate()
            .map(|(idx, layer)| {
                if Some(idx) == last_stable_idx {
                    ContentBlock::text_with_cache(layer.text.clone(), CacheControl::Breakpoint)
                } else {
                    ContentBlock::text(&layer.text)
                }
            })
            .collect();

        Message::System { content }
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
/// 每一层可以独立设置稳定性，最大化前缀缓存命中率。
///
/// # 示例
///
/// ```
/// use lellm_core::Prompt;
///
/// let msg = Prompt::builder()
///     .stable("核心身份…")
///     .stable("工具指南…")
///     .stable("项目规则…")
///     .dynamic("会话上下文: …")
///     .finish()
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

    /// 添加稳定层 — 内容不常变化，参与缓存前缀。
    ///
    /// 用于核心身份、工具指南、项目规则等。
    /// 最后一个 stable 层会自动获得 `CacheControl::Breakpoint`。
    pub fn stable(mut self, text: impl Into<String>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            stable: true,
        });
        self
    }

    /// 添加动态层 — 内容频繁变化，不参与缓存前缀。
    ///
    /// 用于会话上下文、临时注入信息等。
    pub fn dynamic(mut self, text: impl Into<String>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            stable: false,
        });
        self
    }

    /// 构建为 `Prompt`（尚未放置断点）。
    pub fn finish(self) -> Prompt {
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
        let msg = prompt.build();
        assert_eq!(msg.content().len(), 1);
        assert_eq!(msg.content()[0].as_text(), Some("hello"));
        if let ContentBlock::Text(t) = &msg.content()[0] {
            assert!(t.cache_control.is_none());
        }
    }

    #[test]
    fn test_prompt_from_str() {
        let s = "world";
        let prompt: Prompt = s.into();
        let msg = prompt.build();
        assert_eq!(msg.content()[0].as_text(), Some("world"));
    }

    #[test]
    fn test_prompt_plain() {
        let prompt = Prompt::plain("plain text");
        let msg = prompt.build();
        assert_eq!(msg.content().len(), 1);
        assert!(matches!(
            &msg.content()[0],
            ContentBlock::Text(t) if t.cache_control.is_none()
        ));
    }

    #[test]
    fn test_prompt_builder_layered() {
        let msg = Prompt::builder()
            .stable("layer1")
            .stable("layer2")
            .dynamic("dynamic")
            .finish()
            .build();

        let blocks = msg.content();
        assert_eq!(blocks.len(), 3);

        // Layer 1 — stable, but NO breakpoint (not the last stable)
        if let ContentBlock::Text(t) = &blocks[0] {
            assert_eq!(t.text, "layer1");
            assert!(
                t.cache_control.is_none(),
                "Intermediate stable layer should NOT have breakpoint"
            );
        } else {
            panic!("expected Text block");
        }

        // Layer 2 — stable, HAS breakpoint (last stable layer before dynamic)
        if let ContentBlock::Text(t) = &blocks[1] {
            assert_eq!(t.text, "layer2");
            assert!(
                t.cache_control.is_some(),
                "Last stable layer should have breakpoint"
            );
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
    fn test_prompt_is_empty() {
        let empty = Prompt::default();
        assert!(empty.is_empty());

        let nonempty: Prompt = "x".into();
        assert!(!nonempty.is_empty());
    }

    #[test]
    fn test_breakpoint_only_on_last_stable() {
        let msg = Prompt::builder()
            .stable("L1")
            .stable("L2")
            .stable("L3")
            .stable("L4")
            .stable("L5")
            .dynamic("D")
            .finish()
            .build();

        let blocks = msg.content();
        assert_eq!(blocks.len(), 6);

        let breakpoint_count = blocks
            .iter()
            .filter(|b| {
                if let ContentBlock::Text(t) = b {
                    t.cache_control.is_some()
                } else {
                    false
                }
            })
            .count();
        assert_eq!(
            breakpoint_count, 1,
            "Should have exactly 1 breakpoint (on last stable layer)"
        );

        // Verify it's on L5 (index 4)
        if let ContentBlock::Text(t) = &blocks[4] {
            assert!(t.cache_control.is_some());
        }
    }

    #[test]
    fn test_all_stable_single_breakpoint() {
        let msg = Prompt::builder().stable("A").stable("B").finish().build();

        let blocks = msg.content();
        if let ContentBlock::Text(t) = &blocks[0] {
            assert!(t.cache_control.is_none(), "A should not have breakpoint");
        }
        if let ContentBlock::Text(t) = &blocks[1] {
            assert!(t.cache_control.is_some(), "B should have breakpoint");
        }
    }

    #[test]
    fn test_empty_layers_produce_empty_message() {
        let msg = Prompt::builder().finish().build();
        assert!(msg.content().is_empty());
    }

    #[test]
    fn test_flatten_text_ignores_cache_control() {
        let blocks = vec![
            ContentBlock::text_with_cache("cached part".into(), CacheControl::Breakpoint),
            ContentBlock::text("dynamic part"),
        ];
        assert_eq!(
            ContentBlock::flatten_text(&blocks),
            "cached part\n\ndynamic part"
        );
    }

    #[test]
    fn test_flatten_text_single_block() {
        let blocks = vec![ContentBlock::text("hello")];
        assert_eq!(ContentBlock::flatten_text(&blocks), "hello");
    }

    #[test]
    fn test_flatten_text_empty() {
        let blocks: Vec<ContentBlock> = vec![];
        assert_eq!(ContentBlock::flatten_text(&blocks), "");
    }
}
