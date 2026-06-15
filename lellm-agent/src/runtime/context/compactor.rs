//! 上下文压缩器 — 可插拔策略 SPI。
//!
//! 未来可替换为：
//! - `LLMCompactor` — 使用轻量模型生成摘要
//! - `VectorMemoryCompactor` — 基于向量相似度保留关键消息

use lellm_core::Message;

use super::budget::ContextBudget;

/// 压缩操作的结果。
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// 压缩后的消息列表
    pub messages: Vec<Message>,
    /// 压缩前的 Token 数
    pub before_tokens: usize,
    /// 压缩后的 Token 数
    pub after_tokens: usize,
    /// 被移除的消息数量
    pub removed_messages: usize,
}

/// 上下文压缩器 — 可插拔策略。
pub trait ContextCompactor: Send + Sync {
    /// 对消息列表执行压缩。
    ///
    /// **关键约束：**
    /// Assistant(tool_call) + 对应的 ToolResult 是原子块，不可拆分。
    /// 压缩后的历史必须保持 Tool Calling 协议的完整性。
    fn compact(&self, messages: &[Message], budget: &ContextBudget) -> CompactionResult;
}
