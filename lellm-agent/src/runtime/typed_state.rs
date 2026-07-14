//! Agent Typed State + Mutation — 编译期类型安全的 Agent 状态。
//!
//! 替代 `react.rs` 中的 `create_state_from_ctx()` / `sync_state_to_ctx()`
//! 以及 `runtime.rs` 中的全部 State 辅助函数。
//!
//! # 核心原则
//!
//! - `AgentState` 是强类型 struct，不是 `HashMap<String, Value>`
//! - 状态变更通过 `AgentMutation`（领域事件），不是直接修改字段
//! - 并行合并由 Graph 层的 [`AgentStateMerge`]（`MergeStrategy`）决定
//! - 零 JSON 序列化开销（节点直接操作 typed state）

use lellm_core::{ChatResponse, Message};
use lellm_derive::StateMutation;
use lellm_graph::WorkflowState;
use serde::{Deserialize, Serialize};

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
    /// 用于 Mutation 未 apply 时的预判（节点 emit Mutation 之前）。
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

// ─── AgentMutation ────────────────────────────────────────────────

/// Agent 领域事件 — 描述一次状态转换。
///
/// 节点通过发射 Mutation 来变更状态，而非直接修改 `AgentState` 字段。
/// Mutation 是可序列化的、自包含的、不可变的。
///
/// `StateMutation` derive 自动生成 `apply()` 方法。
/// 每个 variant 的 `#[mutation(...)]` 指定了对 `state` 的操作表达式。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, StateMutation)]
#[state(AgentState)]
pub enum AgentMutation {
    /// 追加一条消息到历史
    #[mutation(state.messages.push(value))]
    AppendMessage(Message),
    /// 追加多条消息到历史
    #[mutation(state.messages.extend(value))]
    AppendMessages(Vec<Message>),
    /// 进入下一轮迭代
    #[mutation(state.iterations += 1)]
    IncrementIteration,
    /// 记录工具调用数量
    #[mutation(state.total_tool_calls += value)]
    AddToolCalls(usize),
    /// 记录输出 Token
    #[mutation(state.output_tokens += value)]
    AddOutputTokens(usize),
    /// 记录推理 Token
    #[mutation(state.reasoning_tokens += value)]
    AddReasoningTokens(usize),
    /// 记录一次压缩
    #[mutation(state.compact_count += 1)]
    IncrementCompactCount,
    /// 替换消息历史（压缩场景）
    #[mutation(state.messages = value)]
    ReplaceMessages(Vec<Message>),
    /// 设置停止原因
    #[mutation(state.stop_reason = Some(value))]
    SetStopReason(StopReason),
    /// 更新最后一次 LLM 响应
    #[mutation(state.last_response = Some(value))]
    SetLastResponse(ChatResponse),
}

// ─── AgentCheckpoint ─────────────────────────────────────────────

/// Agent Checkpoint 投影 — 可序列化的快照。
///
/// 只包含需要持久化的字段，不包含运行时字段（如 `last_response`）。
/// 这是 P0-1 Checkpoint Projection 的核心：Runtime State 可以包含
/// 不可序列化字段，Checkpoint 只序列化必要字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCheckpoint {
    /// 消息历史
    pub messages: Vec<Message>,
    /// 当前迭代轮次
    pub iterations: usize,
    /// 累计工具调用总数
    pub total_tool_calls: usize,
    /// 累计输出 Token 数
    pub output_tokens: usize,
    /// 累计推理 Token 数
    pub reasoning_tokens: usize,
    /// 累计压缩次数
    pub compact_count: usize,
    /// 停止原因
    pub stop_reason: Option<StopReason>,
    // 不包含: last_response（可重建）, Arc<dyn ...>, Sender 等
}

// ─── WorkflowState for AgentState ───────────────────────────────

impl WorkflowState for AgentState {
    type Checkpoint = AgentCheckpoint;
    type Mutation = AgentMutation;

    fn snapshot(&self) -> AgentCheckpoint {
        AgentCheckpoint {
            messages: self.messages.clone(),
            iterations: self.iterations,
            total_tool_calls: self.total_tool_calls,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            compact_count: self.compact_count,
            stop_reason: self.stop_reason.clone(),
        }
    }

    fn restore(checkpoint: AgentCheckpoint) -> Self {
        AgentState {
            messages: checkpoint.messages,
            iterations: checkpoint.iterations,
            total_tool_calls: checkpoint.total_tool_calls,
            output_tokens: checkpoint.output_tokens,
            reasoning_tokens: checkpoint.reasoning_tokens,
            compact_count: checkpoint.compact_count,
            stop_reason: checkpoint.stop_reason,
            last_response: None, // 重建时为空，下次 LLM 调用会填充
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
#[derive(Clone)]
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

// ─── NOTE: 序列化桥接已删除 ─────────────────────────
// to_value() / from_value() / apply_from_value() / AGENT_STATE_KEY
// 已删除 — Graph 层只做 Node → Mutation → State 管道，不经过 JSON。
// 如需 Checkpoint 持久化，由 Checkpoint 层直接序列化 AgentState。
