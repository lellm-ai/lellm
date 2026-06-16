//! Checkpoint + ExecutionTrace — 执行恢复与审计追踪。
//!
//! **Checkpoint 负责恢复，ExecutionTrace 负责审计。** 两者是完全独立的对象。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delta::StateDelta;
use crate::state::State;

// ─── Checkpoint ─────────────────────────────────────────────────

/// Checkpoint ID — 唯一标识一个快照。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub uuid::Uuid);

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 执行游标 — 标识当前执行到哪个节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Checkpoint — Materialized State + Execution Cursor
///
/// 保存完整的物化状态快照，恢复时无需 replay，直接 load + continue。
/// 不知道 ReducerRegistry，不知道 Delta 序列。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 快照唯一 ID
    pub checkpoint_id: CheckpointId,
    /// 关联的原始执行 Trace ID
    pub parent_trace_id: TraceId,
    /// 图结构快照 hash（用于变更校验）
    pub graph_hash: String,
    /// 执行游标 — 从哪个节点继续
    pub current_node: NodeId,
    /// 完整物化快照（所有 Delta 已 apply）
    pub state: State,
    /// 创建时间
    pub created_at: String,
}

impl Checkpoint {
    pub fn new(
        parent_trace_id: TraceId,
        graph_hash: impl Into<String>,
        current_node: impl Into<String>,
        state: State,
    ) -> Self {
        Self {
            checkpoint_id: CheckpointId(uuid::Uuid::new_v4()),
            parent_trace_id,
            graph_hash: graph_hash.into(),
            current_node: NodeId(current_node.into()),
            state,
            created_at: chrono_like_timestamp(),
        }
    }
}

/// 图变更校验模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphHashMode {
    /// 严格模式 — hash 不同则拒绝恢复
    Strict,
    /// 强制模式 — hash 不同则 warn + 继续
    Force,
}

// ─── ExecutionTrace ─────────────────────────────────────────────

/// Trace ID — 唯一标识一次完整的图执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// 节点执行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEntry {
    /// 执行步骤序号
    pub step: usize,
    /// 节点名称
    pub node_name: String,
    /// 开始时间
    pub start_time: String,
    /// 结束时间
    pub end_time: String,
    /// 是否成功
    pub success: bool,
    /// 错误信息（如果失败）
    pub error: Option<String>,
}

/// Barrier 决策记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BarrierDecision {
    /// Barrier ID
    pub barrier_id: String,
    /// 节点名称
    pub node_id: String,
    /// 决策结果
    pub decision: Value,
    /// 决策时间
    pub decided_at: String,
}

/// ExecutionTrace — Delta 历史
///
/// 记录执行的完整历史，用于可视化、调试、审计。
/// Delta 只存在于 ExecutionTrace，不进入 Checkpoint。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    /// 执行追踪 ID
    pub trace_id: TraceId,
    /// 初始状态
    pub initial_state: State,
    /// 节点执行记录
    pub entries: Vec<ExecutionEntry>,
    /// 每个节点的修改意图
    pub deltas: Vec<StateDelta>,
    /// Barrier 决策历史
    pub barrier_decisions: Vec<BarrierDecision>,
}

impl ExecutionTrace {
    pub fn new(initial_state: State) -> Self {
        Self {
            trace_id: TraceId::default(),
            initial_state,
            entries: Vec::new(),
            deltas: Vec::new(),
            barrier_decisions: Vec::new(),
        }
    }
}

/// 图执行最终结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphResult {
    /// 执行追踪 ID
    pub trace_id: TraceId,
    /// 最终状态
    pub state: State,
    /// 执行日志
    pub execution_log: Vec<ExecutionEntry>,
    /// 总耗时（毫秒）
    pub duration_ms: u128,
}

// ─── Helpers ────────────────────────────────────────────────────

/// 简单的 ISO 8601 时间戳（无外部依赖）。
fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Simple ISO 8601-like format
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        ((secs / 86400 / 365) + 1970) as u16,
        ((secs / 86400 % 365) / 30 + 1) as u8,
        (secs / 86400 % 30 + 1) as u8,
        (secs % 86400 / 3600) as u8,
        (secs % 3600 / 60) as u8,
        (secs % 60) as u8
    )
}
