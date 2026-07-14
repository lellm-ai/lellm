//! AgentState 的 LeafContext 扩展 — 业务语义便捷方法。
//!
//! 让节点代码写 `ctx.append_message(msg)` 而不是
//! `ctx.record(AgentMutation::AppendMessage(msg))`。

use lellm_core::{ChatResponse, Message};
use lellm_graph::LeafContext;

use super::typed_state::{AgentMutation, AgentState};
use super::event::StopReason;

/// AgentState 的 LeafContext 扩展 trait。
///
/// 提供业务语义的便捷方法，封装 `ctx.record(AgentMutation::...)`。
pub trait AgentContextExt {
    /// 追加一条消息到历史。
    fn append_message(&mut self, msg: Message);

    /// 追加多条消息到历史。
    fn append_messages(&mut self, msgs: Vec<Message>);

    /// 替换消息历史（压缩场景）。
    fn replace_messages(&mut self, msgs: Vec<Message>);

    /// 进入下一轮迭代。
    fn increment_iteration(&mut self);

    /// 记录工具调用数量。
    fn add_tool_calls(&mut self, n: usize);

    /// 记录输出 Token。
    fn add_output_tokens(&mut self, n: usize);

    /// 记录推理 Token。
    fn add_reasoning_tokens(&mut self, n: usize);

    /// 记录一次压缩。
    fn increment_compact_count(&mut self);

    /// 设置停止原因。
    fn set_stop_reason(&mut self, reason: StopReason);

    /// 更新最后一次 LLM 响应。
    fn set_last_response(&mut self, response: ChatResponse);
}

impl AgentContextExt for LeafContext<'_, AgentState> {
    fn append_message(&mut self, msg: Message) {
        self.record(AgentMutation::AppendMessage(msg));
    }

    fn append_messages(&mut self, msgs: Vec<Message>) {
        self.record(AgentMutation::AppendMessages(msgs));
    }

    fn replace_messages(&mut self, msgs: Vec<Message>) {
        self.record(AgentMutation::ReplaceMessages(msgs));
    }

    fn increment_iteration(&mut self) {
        self.record(AgentMutation::IncrementIteration);
    }

    fn add_tool_calls(&mut self, n: usize) {
        self.record(AgentMutation::AddToolCalls(n));
    }

    fn add_output_tokens(&mut self, n: usize) {
        self.record(AgentMutation::AddOutputTokens(n));
    }

    fn add_reasoning_tokens(&mut self, n: usize) {
        self.record(AgentMutation::AddReasoningTokens(n));
    }

    fn increment_compact_count(&mut self) {
        self.record(AgentMutation::IncrementCompactCount);
    }

    fn set_stop_reason(&mut self, reason: StopReason) {
        self.record(AgentMutation::SetStopReason(reason));
    }

    fn set_last_response(&mut self, response: ChatResponse) {
        self.record(AgentMutation::SetLastResponse(response));
    }
}
