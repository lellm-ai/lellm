//! 上下文预算配置 — 控制 Agent Loop 中 messages 的 Token 总量。

/// 上下文预算配置。
///
/// 控制 Agent Loop 中消息历史的 Token 上限与压缩行为。
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// 消息历史的最大 Token 数（默认 128k）。
    ///
    /// **v0.1**: 固定默认值 128k，适用于大多数模型
    /// **v0.2**: 从 `ResolvedModel.context_window` 自动推导（window * 0.8）
    pub max_tokens: usize,
    /// 达到此占比时触发压缩（默认 0.8 = 80%）
    pub warning_ratio: f32,
    /// 压缩时保留最近多少个 Turn（默认 5）
    pub keep_recent_turns: usize,
    /// 单条工具结果的最大字符数（默认 4096）
    pub max_tool_result_chars: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            warning_ratio: 0.8,
            keep_recent_turns: 5,
            max_tool_result_chars: 4096,
        }
    }
}

impl ContextBudget {
    /// 判断是否需要压缩。
    pub fn should_compact(&self, current_tokens: usize) -> bool {
        let threshold = (self.max_tokens as f32 * self.warning_ratio) as usize;
        current_tokens > threshold
    }

    /// 截断单条工具结果，防止单条响应撑爆上下文。
    pub fn truncate_tool_result(&self, text: String) -> String {
        if text.chars().count() <= self.max_tool_result_chars {
            return text;
        }
        let truncated: String = text.chars().take(self.max_tool_result_chars).collect();
        format!(
            "{}\n[truncated, original {} chars]",
            truncated,
            text.chars().count()
        )
    }
}
