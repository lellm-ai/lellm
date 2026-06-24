//! Agent Typed State + Effect — 编译期类型安全的 Agent 状态。
//!
//! 替代 `react.rs` 中的 `create_state_from_ctx()` / `sync_state_to_ctx()`
//! 以及 `runtime.rs` 中的全部 State 辅助函数。
//!
//! # 核心原则
//!
//! - `AgentState` 是强类型 struct，不是 `HashMap<String, Value>`
//! - 状态变更通过 `AgentEffect`（领域事件），不是直接修改字段
//! - 并行合并由 Graph 层的 [`AgentStateMerge`]（`MergeStrategy`）决定
//! - 零 JSON 序列化开销（节点直接操作 typed state）

use lellm_core::{ChatResponse, Message};
use lellm_graph::WorkflowState;

use super::context::estimate_tokens;
use super::event::StopReason;

// ─── AgentState ─────────────────────────────────────────────────

/// Agent 类型化状态。
///
/// 替代 `HashMap<String, Value>` 中通过 `StateKey<T>` 存取的模式。
/// 所有字段编译期可见，零运行时类型检查开销。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentState {
    /// 消息历史（Assistant + Tool + User）
    pub messages: Vec<Message>,
    /// 当前迭代轮次
    pub iterations: usize,
    /// 累计工具调用总数
    pub total_tool_calls: usize,
    /// 累计输出 Token 数（Text，不含 Thinking）
    pub output_tokens: usize,
    /// 累计推理 Token 数（Thinking，不含 Text）
    pub reasoning_tokens: usize,
    /// 累计压缩次数
    pub compact_count: usize,
    /// 停止原因（终态）
    pub stop_reason: Option<StopReason>,
    /// 最后一次 LLM 响应
    pub last_response: Option<ChatResponse>,
}

impl AgentState {
    /// 从初始消息列表创建 AgentState。
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            iterations: 0,
            total_tool_calls: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            compact_count: 0,
            stop_reason: None,
            last_response: None,
        }
    }

    /// 实时派生上下文 Token 数（从 messages 估算）。
    /// 供 BudgetCondition 判断是否需要压缩。
    pub fn estimated_context_tokens(&self) -> usize {
        estimate_tokens(&self.messages)
    }

    /// 检查是否已达到最大迭代轮次。
    pub fn reached_max(&self, max_iterations: usize) -> bool {
        self.iterations >= max_iterations
    }

    /// 检查是否超过总输出预算。
    pub fn exceeded_output(&self, max: Option<u32>) -> bool {
        match max {
            Some(limit) => self.output_tokens >= limit as usize,
            None => false,
        }
    }

    /// 检查是否超过总推理预算。
    pub fn exceeded_reasoning(&self, max: Option<u32>) -> bool {
        match max {
            Some(limit) => self.reasoning_tokens >= limit as usize,
            None => false,
        }
    }

    /// 检查加上额外 Token 后是否超过总输出预算。
    ///
    /// 用于 Effect 未 apply 时的预判（节点 emit Effect 之前）。
    pub fn exceeded_output_with_extra(&self, max: Option<u32>, extra: usize) -> bool {
        match max {
            Some(limit) => self.output_tokens + extra >= limit as usize,
            None => false,
        }
    }

    /// 检查加上额外 Token 后是否超过总推理预算。
    pub fn exceeded_reasoning_with_extra(&self, max: Option<u32>, extra: usize) -> bool {
        match max {
            Some(limit) => self.reasoning_tokens + extra >= limit as usize,
            None => false,
        }
    }

    /// 检查是否已终止（有 stop_reason）。
    pub fn is_terminal(&self) -> bool {
        self.stop_reason.is_some()
    }

    /// 获取当前消息引用。
    pub fn messages_ref(&self) -> &[Message] {
        &self.messages
    }
}

// ─── AgentEffect ────────────────────────────────────────────────

/// Agent 领域事件 — 描述一次状态转换。
///
/// 节点通过发射 Effect 来变更状态，而非直接修改 `AgentState` 字段。
/// Effect 是可序列化的、自包含的、不可变的。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AgentEffect {
    /// 追加一条消息到历史
    AppendMessage(Message),
    /// 追加多条消息到历史
    AppendMessages(Vec<Message>),
    /// 进入下一轮迭代
    IncrementIteration,
    /// 记录工具调用数量
    AddToolCalls(usize),
    /// 记录输出 Token
    AddOutputTokens(usize),
    /// 记录推理 Token
    AddReasoningTokens(usize),
    /// 记录一次压缩
    IncrementCompactCount,
    /// 替换消息历史（压缩场景）
    ReplaceMessages(Vec<Message>),
    /// 设置停止原因
    SetStopReason(StopReason),
    /// 更新最后一次 LLM 响应
    SetLastResponse(ChatResponse),
}

impl lellm_graph::Effect for AgentEffect {}

// ─── WorkflowState for AgentState ───────────────────────────────

impl WorkflowState for AgentState {
    type Effect = AgentEffect;

    fn apply(&mut self, effect: Self::Effect) {
        match effect {
            AgentEffect::AppendMessage(msg) => {
                self.messages.push(msg);
            }
            AgentEffect::AppendMessages(msgs) => {
                self.messages.extend(msgs);
            }
            AgentEffect::IncrementIteration => {
                self.iterations += 1;
            }
            AgentEffect::AddToolCalls(n) => {
                self.total_tool_calls += n;
            }
            AgentEffect::AddOutputTokens(n) => {
                self.output_tokens += n;
            }
            AgentEffect::AddReasoningTokens(n) => {
                self.reasoning_tokens += n;
            }
            AgentEffect::IncrementCompactCount => {
                self.compact_count += 1;
            }
            AgentEffect::ReplaceMessages(msgs) => {
                self.messages = msgs;
            }
            AgentEffect::SetStopReason(reason) => {
                self.stop_reason = Some(reason);
            }
            AgentEffect::SetLastResponse(response) => {
                self.last_response = Some(response);
            }
        }
    }
}

/// AgentState 的默认合并策略。
///
/// - messages: 所有分支拼接（chain）
/// - iterations: 取最大值
/// - total_tool_calls: 取最大值
/// - output_tokens: 累加
/// - reasoning_tokens: 累加
/// - compact_count: 累加
/// - stop_reason: 优先取后者
/// - last_response: 优先取后者
pub struct AgentStateMerge;

impl lellm_graph::MergeStrategy<AgentState> for AgentStateMerge {
    fn merge(branches: Vec<AgentState>) -> Result<AgentState, lellm_graph::WorkflowError> {
        let mut iter = branches.into_iter();
        let mut merged = iter.next().ok_or_else(|| {
            lellm_graph::WorkflowError::MergeConflict("no branches to merge".into())
        })?;

        for branch in iter {
            merged.messages.extend(branch.messages);
            merged.iterations = merged.iterations.max(branch.iterations);
            merged.total_tool_calls = merged.total_tool_calls.max(branch.total_tool_calls);
            merged.output_tokens += branch.output_tokens;
            merged.reasoning_tokens += branch.reasoning_tokens;
            merged.compact_count += branch.compact_count;
            if merged.stop_reason.is_none() {
                merged.stop_reason = branch.stop_reason;
            }
            if merged.last_response.is_none() {
                merged.last_response = branch.last_response;
            }
        }

        Ok(merged)
    }

    fn default_instance() -> Self {
        AgentStateMerge
    }
}

// ─── 序列化辅助（用于与 NodeContext 桥接）────────────────────────

/// AgentState 序列化 key（与 NodeContext 桥接时使用）。
pub const AGENT_STATE_KEY: &str = "__agent_state__";

impl AgentState {
    /// 序列化为 serde_json::Value（用于存储到 NodeContext）。
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_default()
    }

    /// 从 serde_json::Value 反序列化（从 NodeContext 读取时使用）。
    pub fn from_value(v: serde_json::Value) -> Option<Self> {
        serde_json::from_value(v).ok()
    }

    /// 从 serde_json::Value 反序列化 AgentEffect 并应用到状态。
    ///
    /// 供 Effect 循环使用：consume_effects → apply_from_value。
    pub fn apply_from_value(
        &mut self,
        v: serde_json::Value,
    ) -> Result<(), lellm_graph::WorkflowError> {
        let effect = serde_json::from_value(v)
            .map_err(|e| lellm_graph::WorkflowError::ApplyFailed(e.to_string()))?;
        self.apply(effect);
        Ok(())
    }
}
