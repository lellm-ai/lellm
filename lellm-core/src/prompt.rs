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

/// Prompt 层 — 一段文本 + 缓存意图标记。
///
/// 内部使用，不对外暴露。用户通过 `PromptBuilder` 操作。
///
/// `is_cached` 表示用户希望这段内容被缓存。
/// 实际的 `CacheControl::Breakpoint` 由 `to_content_blocks()` 统一放置在
/// **最后一个 cached layer** 上（Anthropic 最多 4 个断点，中间断点无意义）。
#[derive(Debug, Clone)]
struct PromptLayer {
    text: String,
    /// 用户标记的缓存意图（非直接映射为 Breakpoint）
    is_cached: bool,
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
/// use lellm_core::Prompt;
///
/// // 简单文本 — 自动转换
/// let simple: Prompt = "You are a helpful assistant.".into();
///
/// // 分层构建 — 最大化前缀缓存
/// // 只有最后一个 cached layer 会获得 cache_control 断点（Anthropic 限额 4 个/请求）
/// let layered = Prompt::builder()
///     .layer_cached("核心身份…")               // 永不变化 — 无断点
///     .layer_cached("工具指南…")               // 极少变化 — 获得断点 ✓
///     .layer_dynamic("会话上下文: …")          // 每轮变化 — 无断点
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
                is_cached: false,
            }],
        }
    }

    /// 创建分层构建器。
    pub fn builder() -> PromptBuilder {
        PromptBuilder::new()
    }

    /// 将 Prompt 转换为带 cache_control 的 `Vec<ContentBlock>`。
    ///
    /// **断点放置策略：** 只在最后一个 cached layer 上放置 `CacheControl::Breakpoint`。
    ///
    /// Anthropic 每个请求最多允许 4 个 `cache_control` 断点。
    /// 缓存前缀是累积的——断点标记的是"到此为止的前缀被缓存"。
    /// 中间层的断点不产生独立的缓存段，纯属浪费限额。
    /// 所以只在最后一个 cached layer（即 dynamic layer 之前的那个）放置断点。
    pub fn to_content_blocks(&self) -> Vec<ContentBlock> {
        // 找到最后一个 cached layer 的索引
        let last_cached_idx = self
            .layers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, layer)| layer.is_cached)
            .map(|(idx, _)| idx);

        self.layers
            .iter()
            .enumerate()
            .map(|(idx, layer)| {
                if Some(idx) == last_cached_idx {
                    ContentBlock::text_with_cache(layer.text.clone(), CacheControl::Breakpoint)
                } else {
                    ContentBlock::text(&layer.text)
                }
            })
            .collect()
    }

    /// 合并所有层为纯文本，层之间以 `\n\n` 分隔。
    ///
    /// 用于不支持 `cache_control` 的 Provider（如 OpenAI、Google）。
    ///
    /// **注意：** `\n\n` 分隔符是 fallback 行为，仅用于纯文本拼接。
    /// Anthropic 路径使用 `to_content_blocks()`，不存在此分隔符差异。
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
/// use lellm_core::Prompt;
///
/// let prompt = Prompt::builder()
///     // L1 — 核心身份，永不变化 → 永远命中缓存（无断点）
///     .layer_cached("你是 DevOps Agent，专注于 CI/CD 管理。")
///     // L2 — 工具指南，极少变化 → 长期命中缓存（无断点）
///     .layer_cached("可用工具: get_time, get_env, get_config")
///     // L3 — 项目规则，偶尔变化（无断点）
///     .layer_cached("项目规则: 使用中文回复。")
///     // L4 — 分隔符（获得断点 ✓ — 最后一个 cached layer）
///     .layer_cached("---")
///     // L5 — 会话上下文，频繁变化 → 不缓存
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

    /// 添加缓存层。
    ///
    /// 用于稳定性高的内容（核心身份、工具指南、项目规则）。
    /// 这是最常用的方法——绝大多数层都应该缓存。
    ///
    /// **注意：** 实际的 `cache_control` 断点由 `to_content_blocks()` 统一放置在
    /// 最后一个 cached layer 上（Anthropic 最多 4 个断点/请求，中间断点无意义）。
    pub fn layer_cached(mut self, text: impl Into<String>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            is_cached: true,
        });
        self
    }

    /// 添加不缓存的层。
    ///
    /// 用于频繁变化的内容（会话上下文、临时注入信息）。
    pub fn layer_dynamic(mut self, text: impl Into<String>) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            is_cached: false,
        });
        self
    }

    /// 添加带自定义缓存策略的层。
    ///
    /// `is_cached = true` 表示希望缓存，`false` 表示不缓存。
    /// 实际的 `cache_control` 断点由 `to_content_blocks()` 统一放置。
    pub fn layer(mut self, text: impl Into<String>, is_cached: bool) -> Self {
        self.layers.push(PromptLayer {
            text: text.into(),
            is_cached,
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

        // Layer 1 — cached, but NO breakpoint (not the last cached)
        if let ContentBlock::Text(t) = &blocks[0] {
            assert_eq!(t.text, "layer1");
            assert!(
                t.cache_control.is_none(),
                "Intermediate cached layer should NOT have breakpoint"
            );
        } else {
            panic!("expected Text block");
        }

        // Layer 2 — cached, HAS breakpoint (last cached layer before dynamic)
        if let ContentBlock::Text(t) = &blocks[1] {
            assert_eq!(t.text, "layer2");
            assert!(
                t.cache_control.is_some(),
                "Last cached layer should have breakpoint"
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
            .layer("cached", true)
            .layer("no cache", false)
            .build();

        let blocks = prompt.to_content_blocks();
        // "cached" is the last (and only) cached layer → gets the breakpoint
        if let ContentBlock::Text(t) = &blocks[0] {
            assert!(
                t.cache_control.is_some(),
                "Only cached layer should get breakpoint"
            );
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
    fn test_breakpoint_only_on_last_cached() {
        // 5 cached layers + 1 dynamic → only layer 5 gets the breakpoint
        let prompt = Prompt::builder()
            .layer_cached("L1")
            .layer_cached("L2")
            .layer_cached("L3")
            .layer_cached("L4")
            .layer_cached("L5")
            .layer_dynamic("D")
            .build();

        let blocks = prompt.to_content_blocks();
        assert_eq!(blocks.len(), 6);

        // Count breakpoints
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
            "Should have exactly 1 breakpoint (on last cached layer)"
        );

        // Verify it's on L5 (index 4)
        if let ContentBlock::Text(t) = &blocks[4] {
            assert!(t.cache_control.is_some());
        }
    }

    #[test]
    fn test_all_cached_single_breakpoint() {
        // All cached, no dynamic → last layer gets breakpoint
        let prompt = Prompt::builder()
            .layer_cached("A")
            .layer_cached("B")
            .build();

        let blocks = prompt.to_content_blocks();
        if let ContentBlock::Text(t) = &blocks[0] {
            assert!(t.cache_control.is_none(), "A should not have breakpoint");
        }
        if let ContentBlock::Text(t) = &blocks[1] {
            assert!(t.cache_control.is_some(), "B should have breakpoint");
        }
    }

    #[test]
    fn test_empty_layers_produce_empty_blocks() {
        let prompt = Prompt::builder().build();
        assert!(prompt.to_content_blocks().is_empty());
        assert_eq!(prompt.build_text(), "");
    }
}
