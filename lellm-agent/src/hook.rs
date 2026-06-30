//! Agent-level Hook — AgentLoop 执行扩展点。
//!
//! v0.4+: Hook 仅用于观测（日志、指标等），不再修改 State。
//! State 变更应通过 Mutation 模型进行。

use crate::runtime::{AgentEvent, ToolUseResult};

/// Agent 执行上下文 — 传递给 Hook 的执行信息。
pub struct AgentHookContext {
    /// Agent 节点名称
    pub node_name: String,
    /// 输入消息数量
    pub input_message_count: usize,
}

/// Agent 执行快照 — after_agent 收到的执行结果。
pub struct AgentHookSnapshot {
    /// 执行结果
    pub result: ToolUseResult,
    /// 收到的事件流（用于审计）
    pub events: Vec<AgentEvent>,
}

/// Agent-level Hook trait。
///
/// 在 Agent 执行前后调用。
/// v0.4+: 仅用于观测，不修改 State。
pub trait AgentHook: Send + Sync {
    /// Agent loop 执行前调用。
    fn before_agent(&self, _ctx: &AgentHookContext) {}

    /// Agent loop 执行后调用。
    fn after_agent(&self, _snapshot: &AgentHookSnapshot) {}
}

/// 无操作 Hook — 默认行为。
#[derive(Debug, Clone, Default)]
pub struct NoOpAgentHook;

impl AgentHook for NoOpAgentHook {}

/// 日志 Hook — 将 agent 执行事件输出为 tracing 日志。
#[derive(Debug, Clone)]
pub struct TracingAgentHook;

impl AgentHook for TracingAgentHook {
    fn before_agent(&self, ctx: &AgentHookContext) {
        tracing::debug!(
            node = %ctx.node_name,
            input_messages = ctx.input_message_count,
            "agent loop starting"
        );
    }

    fn after_agent(&self, snapshot: &AgentHookSnapshot) {
        tracing::debug!(
            iterations = snapshot.result.iterations,
            tool_calls = snapshot.result.tool_calls_executed,
            stop_reason = ?snapshot.result.stop_reason,
            "agent loop completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_agent_hook() {
        let hook = NoOpAgentHook;
        let ctx = AgentHookContext {
            node_name: "test".to_string(),
            input_message_count: 0,
        };
        hook.before_agent(&ctx);
    }

    #[test]
    fn test_tracing_agent_hook() {
        let hook = TracingAgentHook;
        let ctx = AgentHookContext {
            node_name: "test".to_string(),
            input_message_count: 5,
        };
        hook.before_agent(&ctx);
    }
}
