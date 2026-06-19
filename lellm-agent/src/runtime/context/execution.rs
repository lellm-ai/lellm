//! AgentExecutionContext — Agent 执行期的运行时缓存。
//!
//! 不可持久化，Checkpoint 不保存，Resume 时重建。
//! 用于替代 LoopState.estimated_tokens 等派生数据的缓存。

use lellm_core::Message;

use super::estimate_tokens;

/// Agent 运行时上下文 — 不可持久化的执行期缓存。
#[derive(Debug, Clone)]
pub struct AgentExecutionContext {
    /// 消息历史的估算 Token 数（与 messages 同步更新）
    pub cached_token_count: usize,
}

impl AgentExecutionContext {
    /// 从消息列表创建上下文，初始计算 token 估算值。
    pub fn new(messages: &[Message]) -> Self {
        Self {
            cached_token_count: estimate_tokens(messages),
        }
    }

    /// 新增 token 时累加缓存。
    pub fn add_tokens(&mut self, tokens: usize) {
        self.cached_token_count += tokens;
    }

    /// compact 后重新计算缓存。
    pub fn reset_after_compact(&mut self, messages: &[Message]) {
        self.cached_token_count = estimate_tokens(messages);
    }
}
