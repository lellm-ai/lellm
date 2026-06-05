//! Usage Accumulator — 流式模式下聚合 usage token 计数。
//!
//! 职责：独立于 ToolCallAccumulator，只管理 usage 数据的累积与合并。
//!
//! **为什么独立：**
//! - Anthropic 的 `input_tokens` 在 `message_start` 事件中
//! - Anthropic 的 `output_tokens` 在 `message_delta` 事件中
//! - OpenAI 的完整 usage 在最后一个 chunk（需 `stream_options.include_usage`）
//! - 两者与 tool_call 完全无关，不应混入 ToolCallAccumulator

use lellm_core::TokenUsage;

/// Usage 增量 — 统一格式，吸收所有 Provider 差异。
#[derive(Debug)]
pub enum UsageDelta {
    /// 输入 token 计数（Anthropic message_start 事件）
    InputTokens(u32),
    /// 输出 token 计数（Anthropic message_delta 事件）
    OutputTokens(u32),
    /// 完整 usage（OpenAI 最后一个 chunk 携带）
    Full(TokenUsage),
}

/// Usage 增量组装器 — 独立状态机，可单独测试。
pub struct UsageAccumulator {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

impl UsageAccumulator {
    pub fn new() -> Self {
        Self {
            prompt_tokens: None,
            completion_tokens: None,
        }
    }

    /// 接收 usage 增量
    pub fn push(&mut self, delta: &UsageDelta) {
        match delta {
            UsageDelta::InputTokens(n) => {
                self.prompt_tokens = Some(std::cmp::max(*n, self.prompt_tokens.unwrap_or(0)));
            }
            UsageDelta::OutputTokens(n) => {
                self.completion_tokens = Some(std::cmp::max(*n, self.completion_tokens.unwrap_or(0)));
            }
            UsageDelta::Full(u) => {
                // OpenAI 一次性返回完整 usage，直接覆盖
                self.prompt_tokens = Some(u.prompt_tokens);
                self.completion_tokens = Some(u.completion_tokens);
            }
        }
    }

    /// 完成累积，返回最终的 TokenUsage。
    ///
    /// 如果从未收到任何 usage 数据，返回 `None`。
    pub fn finalize(self) -> Option<TokenUsage> {
        match (self.prompt_tokens, self.completion_tokens) {
            (Some(p), Some(c)) => Some(TokenUsage {
                prompt_tokens: p,
                completion_tokens: c,
                total_tokens: p + c,
            }),
            (Some(p), None) => Some(TokenUsage {
                prompt_tokens: p,
                completion_tokens: 0,
                total_tokens: p,
            }),
            (None, Some(c)) => Some(TokenUsage {
                prompt_tokens: 0,
                completion_tokens: c,
                total_tokens: c,
            }),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_anthropic_style() {
        let mut acc = UsageAccumulator::new();

        // message_start → input_tokens
        acc.push(&UsageDelta::InputTokens(100));

        // message_delta → output_tokens
        acc.push(&UsageDelta::OutputTokens(50));

        let usage = acc.finalize().unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
    }

    #[test]
    fn openai_full_overrides() {
        let mut acc = UsageAccumulator::new();

        acc.push(&UsageDelta::InputTokens(100));

        // OpenAI 一次性返回完整 usage
        acc.push(&UsageDelta::Full(TokenUsage {
            prompt_tokens: 200,
            completion_tokens: 80,
            total_tokens: 280,
        }));

        let usage = acc.finalize().unwrap();
        assert_eq!(usage.prompt_tokens, 200);
        assert_eq!(usage.completion_tokens, 80);
    }

    #[test]
    fn empty_returns_none() {
        let acc = UsageAccumulator::new();
        assert!(acc.finalize().is_none());
    }

    #[test]
    fn partial_input_only() {
        let mut acc = UsageAccumulator::new();
        acc.push(&UsageDelta::InputTokens(42));
        let usage = acc.finalize().unwrap();
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 0);
    }
}
