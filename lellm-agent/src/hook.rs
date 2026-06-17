//! Agent-level Hook — AgentLoop 执行扩展点。
//!
//! 与 graph 层 `AgentHook`（观测型）不同，agent 层 hook 可以：
//! - 在 agent loop 执行前后注入 StateDelta
//! - 观测 agent 执行过程
//!
//! # 设计原则
//!
//! - Hook 方法**可以修改 State**（通过返回 `Vec<StateDelta>`）
//! - Hook 修改经过 Reducer，进入 ExecutionTrace — 无审计盲区
//! - Hook 失败不影响 Agent 执行（降级为 ObservedError）

use lellm_graph::StateDelta;

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
/// 在 AgentFlowNode 执行 Agent Loop 前后调用。
/// Hook 返回的 `Vec<StateDelta>` 会经过 Reducer 合并到 State。
///
/// # 示例
///
/// ```rust,ignore
/// use lellm_agent::hook::{AgentHook, AgentHookContext, AgentHookSnapshot};
/// use lellm_graph::StateDelta;
///
/// struct AuditHook;
///
/// impl AgentHook for AuditHook {
///     fn before_agent(&self, ctx: &AgentHookContext) -> Vec<StateDelta> {
///         vec![StateDelta::put("audit_log", serde_json::json!([
///             format!("agent '{}' started", ctx.node_name)
///         ]))]
///     }
///
///     fn after_agent(&self, snapshot: &AgentHookSnapshot) -> Vec<StateDelta> {
///         vec![StateDelta::put(
///             "audit_log",
///             serde_json::json!([format!("agent completed: {} iterations", snapshot.result.iterations)]),
///         )]
///     }
/// }
/// ```
pub trait AgentHook: Send + Sync {
    /// Agent loop 执行前调用。
    /// 返回的 Deltas 会在 agent loop 启动前 apply 到 State。
    fn before_agent(&self, _ctx: &AgentHookContext) -> Vec<StateDelta> {
        Vec::new()
    }

    /// Agent loop 执行后调用。
    /// 返回的 Deltas 会在 agent loop 完成后 apply 到 State。
    fn after_agent(&self, _snapshot: &AgentHookSnapshot) -> Vec<StateDelta> {
        Vec::new()
    }
}

/// 无操作 Hook — 默认行为。
#[derive(Debug, Clone, Default)]
pub struct NoOpAgentHook;

impl AgentHook for NoOpAgentHook {}

/// 日志 Hook — 将 agent 执行事件输出为 tracing 日志。
#[derive(Debug, Clone)]
pub struct TracingAgentHook;

impl AgentHook for TracingAgentHook {
    fn before_agent(&self, ctx: &AgentHookContext) -> Vec<StateDelta> {
        tracing::debug!(
            node = %ctx.node_name,
            input_messages = ctx.input_message_count,
            "agent loop starting"
        );
        Vec::new()
    }

    fn after_agent(&self, snapshot: &AgentHookSnapshot) -> Vec<StateDelta> {
        tracing::debug!(
            iterations = snapshot.result.iterations,
            tool_calls = snapshot.result.tool_calls_executed,
            stop_reason = ?snapshot.result.stop_reason,
            "agent loop completed"
        );
        Vec::new()
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
        let deltas = hook.before_agent(&ctx);
        assert!(deltas.is_empty());
    }

    #[test]
    fn test_tracing_agent_hook() {
        let hook = TracingAgentHook;
        let ctx = AgentHookContext {
            node_name: "test".to_string(),
            input_message_count: 5,
        };
        let deltas = hook.before_agent(&ctx);
        assert!(deltas.is_empty());
    }
}
