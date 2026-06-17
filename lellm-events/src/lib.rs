//! lellm-events — 事件协议。
//!
//! 执行时遥测与可观测性的统一事件类型。
//! 所有 crate 共享此 crate 的事件定义，避免循环依赖。

use std::time::Duration;

use lellm_runtime::StateDelta;

// ─── TraceId / SpanId ────────────────────────────────────────

/// Trace ID — 唯一标识一次完整的图执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TraceId(pub uuid::Uuid);

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Span ID — 标识一次节点执行的唯一 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SpanId(pub uuid::Uuid);

impl Default for SpanId {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl std::fmt::Display for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── AgentEvent ──────────────────────────────────────────────

/// Agent 层流式事件 — 封闭、强类型、exhaustive match。
///
/// 终态契约：
/// - 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
/// - 异常结束：`LoopError` 恰好一次，然后 channel 关闭
/// - 终态事件后不再发送任何事件
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Provider 层事件
    Provider(lellm_provider::ProviderEvent),
    /// 工具开始执行
    ToolStart { tool_call_id: String, name: String },
    /// 工具执行完成
    ToolEnd {
        tool_call_id: String,
        result: Result<serde_json::Value, lellm_core::ToolError>,
    },
    /// 工具重试（RetryPolicy 触发）
    Retry {
        tool_call_id: String,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    /// 上下文压缩完成（可观测性事件）
    ContextCompacted {
        before_tokens: usize,
        after_tokens: usize,
        removed_messages: usize,
    },
    /// Agent loop 正常结束（恰好一次，后不再发送）
    LoopEnd { result: LoopEndResult },
    /// Agent loop 异常结束（恰好一次，后不再发送）— 不含 messages，消费者自行维护
    LoopError {
        error: lellm_core::LlmError,
        iterations: usize,
    },
}

/// Agent loop 停止原因 — 描述"为什么停止"，而非"响应长什么样"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Agent 已获得最终答案并正常结束
    Complete,
    /// 达到最大轮次
    MaxIterationsReached,
    /// 外部取消（消费者断开、task 终止等）
    Cancelled,
    /// 输出预算超限（单轮或总输出 token 超过限制）
    OutputBudgetExceeded,
    /// 推理预算超限（thinking token 超过限制）
    ReasoningBudgetExceeded,
}

/// Agent loop 结束结果（简化版，不含完整 messages）
#[derive(Debug, Clone)]
pub struct LoopEndResult {
    pub stop_reason: StopReason,
    pub iterations: usize,
    pub tool_calls_executed: usize,
}

// ─── FlowEvent ───────────────────────────────────────────────

/// 节点内部事件 — 解耦的通用事件中间层。
///
/// Graph 不知道 `AgentEvent`、`ToolCall`、`ToolResult`。
/// 具体节点（如 AgentFlowNode）通过 `Custom` 变体注入内部事件，
/// 使用 `Box<dyn Any>` 保证类型安全的向下转换。
#[derive(Debug)]
pub enum FlowEvent {
    /// 节点开始执行
    NodeStarted { node_id: String, span_id: SpanId },
    /// 节点执行完成
    NodeCompleted {
        node_id: String,
        span_id: SpanId,
        duration: Duration,
    },
    /// 节点执行失败
    NodeFailed { node_id: String, error: String },
    /// 状态变更
    StateChanged {
        node_id: String,
        delta: StateDelta,
    },
    /// 并行节点开始执行
    ParallelStarted {
        node_id: String,
        branch_count: usize,
        span_id: SpanId,
    },
    /// 并行节点执行完成
    ParallelCompleted {
        node_id: String,
        span_id: SpanId,
        duration: Duration,
    },
    /// 并行分支执行完成
    BranchCompleted {
        branch_name: String,
        node_id: String,
        span_id: SpanId,
        success: bool,
        duration: Duration,
    },
    /// 自定义事件 — 具体节点类型通过此变体注入内部事件。
    ///
    /// 使用 `Box<dyn Any>` 保证类型安全：消费者通过 `downcast_ref::<T>()`
    /// 获取具体类型，无需 serde_json::Value 字符串匹配。
    Custom {
        node_id: String,
        payload: Box<dyn std::any::Any + Send + Sync>,
    },
}

// ─── GraphEvent ──────────────────────────────────────────────

/// Graph 层流式事件 — 封闭、强类型、exhaustive match。
///
/// 事件流生命周期：
/// - 正常结束：`GraphComplete` 恰好一次，然后 channel 关闭
/// - 异常结束：`GraphError` 恰好一次，然后 channel 关闭
/// - 终态事件后不再发送任何事件
#[derive(Debug)]
pub enum GraphEvent {
    /// Graph 执行开始（恰好一次）
    GraphStart { trace_id: TraceId },
    /// 节点开始执行
    NodeStart {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        step: usize,
    },
    /// 节点执行完成
    NodeEnd {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        success: bool,
        duration: Duration,
    },
    /// 节点内部事件（通过 FlowEvent 中间层）
    Node {
        span_id: SpanId,
        node_name: String,
        event: FlowEvent,
    },
    /// Barrier 暂停 — 等待外部审批信号。
    ///
    /// ⚠️ **必须处理** — 如果不发送决策，Graph 执行将永久阻塞。
    BarrierWaiting {
        barrier_id: BarrierId,
        node_name: String,
        span_id: SpanId,
    },
    /// Barrier 决策已应用
    BarrierResolved {
        barrier_id: BarrierId,
        decision: BarrierDecision,
    },
    /// 观测错误 — 不影响 control flow
    ObservedError {
        error: ObservedError,
        node_name: String,
    },
    /// Checkpoint 已保存。
    CheckpointSaved {
        checkpoint_id: lellm_runtime::CheckpointId,
        node_name: String,
        step: usize,
    },
    /// Graph 执行完成（恰好一次）
    GraphComplete { result: GraphCompleteResult },
    /// Graph 执行出错（恰好一次）
    GraphError { error: GraphError, state: StateSnapshot },
}

// ─── BarrierId / BarrierDecision ─────────────────────────────

/// Barrier 审批请求的唯一标识。
///
/// 由 `(node_id, occurrence)` 组成，支持通配决策。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BarrierId {
    /// 用户定义的 Barrier 节点名（可预测）
    pub node_id: String,
    /// 第几次到达（1-based）
    pub occurrence: u32,
}

impl BarrierId {
    pub fn new(node_id: impl Into<String>, occurrence: u32) -> Self {
        Self {
            node_id: node_id.into(),
            occurrence,
        }
    }
}

/// Barrier 审批决策。
#[derive(Debug, Clone)]
pub enum BarrierDecision {
    /// 通过 — 节点继续执行下一步
    Approve,
    /// 拒绝 — 写入拒绝原因到 State，由 edge_if 决定是否回跳
    Reject { reason: String },
    /// 修改 State 中的指定 key，然后继续
    Modify {
        key: String,
        value: serde_json::Value,
    },
    /// 跳转到指定节点（覆盖默认流转）
    Reroute { target: String },
}

// ─── Error Types ─────────────────────────────────────────────

/// 观测错误 — 不影响 control flow，仅用于可观测性。
#[derive(Debug, Clone)]
pub enum ObservedError {
    /// 节点执行降级（如 fallback 路径）
    Degraded {
        node: String,
        reason: String,
    },
    /// 工具调用失败但被忽略
    ToolIgnored {
        tool: String,
        error: String,
    },
    /// 自定义观测错误
    Custom {
        kind: String,
        detail: String,
    },
}

impl std::fmt::Display for ObservedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObservedError::Degraded { node, reason } => {
                write!(f, "degraded: {} ({})", node, reason)
            }
            ObservedError::ToolIgnored { tool, error } => {
                write!(f, "tool ignored: {} ({})", tool, error)
            }
            ObservedError::Custom { kind, detail } => {
                write!(f, "{}: {}", kind, detail)
            }
        }
    }
}

/// 终止错误 — 导致 Graph 执行终止。
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// 节点执行失败
    #[error("node '{node}' failed: {source}")]
    NodeExecutionFailed {
        node: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Barrier 被取消
    #[error("barrier '{node}' cancelled")]
    BarrierCancelled { node: String },

    /// 图结构无效
    #[error("invalid graph: {0}")]
    InvalidGraph(String),

    /// 步数超限
    #[error("max steps ({0}) exceeded")]
    MaxStepsExceeded(usize),

    /// 状态错误
    #[error("state error: {0}")]
    State(#[from] lellm_runtime::StateError),
}

/// Graph 执行最终结果（简化版，不含完整 State）。
#[derive(Debug, Clone)]
pub struct GraphCompleteResult {
    pub trace_id: TraceId,
    pub duration: Duration,
}

/// State 快照（简化版，仅用于 GraphError 携带）。
#[derive(Debug, Clone, Default)]
pub struct StateSnapshot(pub serde_json::Value);

impl StateSnapshot {
    pub fn new() -> Self {
        Self(serde_json::Value::Null)
    }
}

// ─── Agent Event Adapter ─────────────────────────────────────

/// AgentEvent → FlowEvent 适配器。
///
/// 将 AgentEvent 包装为 FlowEvent::Custom，注入 Graph 事件流。
pub fn agent_event_to_flow_event(node_id: &str, event: AgentEvent) -> FlowEvent {
    FlowEvent::Custom {
        node_id: node_id.to_string(),
        payload: Box::new(event),
    }
}

/// 从 FlowEvent 中提取 AgentEvent（如果存在）。
pub fn extract_agent_event(event: &FlowEvent) -> Option<&AgentEvent> {
    match event {
        FlowEvent::Custom { payload, .. } => {
            payload.downcast_ref::<AgentEvent>()
        }
        _ => None,
    }
}
