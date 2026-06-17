//! Checkpoint + ExecutionTrace — 执行恢复与审计追踪。
//!
//! **Checkpoint 负责恢复，ExecutionTrace 负责审计。** 两者是完全独立的对象。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delta::StateDelta;
use crate::state::State;

// ─── CheckpointPolicy ──────────────────────────────────────────

/// Checkpoint 触发时机。
///
/// Checkpoint 是图级执行策略，不是节点属性。
/// 价值公式：`Checkpoint 价值 = 重算成本 × 失败概率`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointTrigger {
    /// Barrier 合并后 — 默认开启，并行分支合并点是天然恢复点
    BarrierResolved,
    /// 执行完成时 — 默认开启，最终结果 = 最后一个 Checkpoint
    ExecutionCompleted,
    /// 人类决策后 — 强烈建议，审批后立即存，避免恢复时重复请求审批
    HumanDecision,
    /// 显式标注 — builder.node("agent", agent).checkpoint() 触发
    Explicit,
    /// 自适应（v0.4）— 基于 ExecutionMetadata 动态决策
    Adaptive,
}

/// Checkpoint 策略 — 图级执行策略，不是节点属性。
///
/// 决定何时自动保存 Checkpoint。
#[derive(Debug, Clone)]
pub struct CheckpointPolicy {
    /// 启用的触发器列表
    pub triggers: Vec<CheckpointTrigger>,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self::conservative()
    }
}

impl CheckpointPolicy {
    /// 保守策略：BarrierResolved + ExecutionCompleted + HumanDecision
    pub fn conservative() -> Self {
        Self {
            triggers: vec![
                CheckpointTrigger::BarrierResolved,
                CheckpointTrigger::ExecutionCompleted,
                CheckpointTrigger::HumanDecision,
            ],
        }
    }

    /// 最小策略：仅 BarrierResolved + ExecutionCompleted
    pub fn minimal() -> Self {
        Self {
            triggers: vec![
                CheckpointTrigger::BarrierResolved,
                CheckpointTrigger::ExecutionCompleted,
            ],
        }
    }

    /// 手动策略：仅显式触发
    pub fn manual() -> Self {
        Self {
            triggers: vec![CheckpointTrigger::Explicit],
        }
    }

    /// 检查是否应该在 BarrierResolved 时保存
    pub fn should_checkpoint_on_barrier(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::BarrierResolved)
    }

    /// 检查是否应该在 ExecutionCompleted 时保存
    pub fn should_checkpoint_on_completion(&self) -> bool {
        self.triggers
            .contains(&CheckpointTrigger::ExecutionCompleted)
    }

    /// 检查是否应该在 HumanDecision 时保存
    pub fn should_checkpoint_on_human_decision(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::HumanDecision)
    }

    /// 检查是否应该在显式触发时保存
    pub fn should_checkpoint_on_explicit(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::Explicit)
    }

    /// 检查是否应该在自适应模式下保存
    pub fn should_checkpoint_adaptive(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::Adaptive)
    }
}

// ─── ExecutionMetadata ────────────────────────────────────────

/// 节点执行元数据 — 用于 Adaptive Checkpoint 决策。
///
/// 每个节点执行完成后收集此元数据，
/// 用于计算 CheckpointScore 决定是否保存。
#[derive(Debug, Clone, Default)]
pub struct ExecutionMetadata {
    /// 执行耗时（毫秒）
    pub duration_ms: u64,
    /// Token 消耗成本（0.0 表示无 LLM 调用）
    pub token_cost: f64,
    /// 是否有外部副作用（如部署、发送消息）
    pub has_side_effects: bool,
}

impl ExecutionMetadata {
    /// TaskNode 等轻量节点的默认元数据。
    pub fn lightweight() -> Self {
        Self {
            duration_ms: 2,
            token_cost: 0.0,
            has_side_effects: false,
        }
    }

    /// AgentNode 等重量级节点的默认元数据。
    pub fn heavy() -> Self {
        Self {
            duration_ms: 90_000, // 90 秒
            token_cost: 0.01,    // 约 1 万 token
            has_side_effects: false,
        }
    }

    /// 有副作用的节点（如部署）的默认元数据。
    pub fn with_side_effects() -> Self {
        Self {
            duration_ms: 0,
            token_cost: 0.0,
            has_side_effects: true,
        }
    }
}

/// Checkpoint 评分 — 基于 ExecutionMetadata 动态决策。
///
/// 评分公式：
/// ```text
/// score = duration_weight * duration_ms
///       + token_weight * token_cost
///       + side_effect_weight * has_side_effects
/// ```
///
/// score >= threshold → 保存 Checkpoint
#[derive(Debug, Clone)]
pub struct CheckpointScore {
    /// 执行耗时的权重（默认 1.0）
    pub duration_weight: f64,
    /// Token 成本的权重（默认 1000.0 — 1 万 token ≈ 1 分钟）
    pub token_weight: f64,
    /// 副作用的权重（默认 10000.0 — 一定保存）
    pub side_effect_weight: f64,
    /// 保存阈值（默认 100.0 — 约 100ms 或 100 token）
    pub threshold: f64,
}

impl Default for CheckpointScore {
    fn default() -> Self {
        Self {
            duration_weight: 1.0,
            token_weight: 1000.0,
            side_effect_weight: 10000.0,
            threshold: 100.0,
        }
    }
}

impl CheckpointScore {
    /// 计算 Checkpoint 评分。
    ///
    /// score >= threshold → 应该保存
    pub fn calculate(&self, metadata: &ExecutionMetadata) -> f64 {
        let mut score = self.duration_weight * metadata.duration_ms as f64;
        score += self.token_weight * metadata.token_cost;
        if metadata.has_side_effects {
            score += self.side_effect_weight;
        }
        score
    }

    /// 判断是否应该保存 Checkpoint。
    pub fn should_checkpoint(&self, metadata: &ExecutionMetadata) -> bool {
        self.calculate(metadata) >= self.threshold
    }
}

// ─── CheckpointStoreError ──────────────────────────────────────

/// Checkpoint 存储操作错误。
#[derive(Debug, thiserror::Error)]
pub enum CheckpointStoreError {
    /// 存储层错误（I/O、网络等）
    #[error("storage error: {0}")]
    Storage(String),
    /// Checkpoint 不存在
    #[error("checkpoint not found: {0}")]
    NotFound(CheckpointId),
    /// Checkpoint 数据损坏
    #[error("corrupted checkpoint: {0}")]
    Corrupted(String),
}

// ─── CheckpointStore trait ─────────────────────────────────────

/// Checkpoint 存储后端 SPI。
///
/// 后端的职责：
/// - 持久化 Checkpoint 快照
/// - 支持按 ID 精确加载
/// - 支持按 trace_id 查找最新快照
/// - 支持列出、删除、清理过期快照
///
/// **不知道** ReducerRegistry、Delta 序列、Graph 结构。
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    /// 保存 Checkpoint。
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointStoreError>;

    /// 加载指定 ID 的 Checkpoint。
    async fn load(&self, id: &CheckpointId) -> Result<Option<Checkpoint>, CheckpointStoreError>;

    /// 按 trace_id 查找最新 Checkpoint（按 created_at 倒序）。
    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<Checkpoint>, CheckpointStoreError>;

    /// 列出 trace_id 下的所有 Checkpoint ID（按时间倒序）。
    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError>;

    /// 删除指定 Checkpoint。返回 `true` 表示已删除，`false` 表示不存在。
    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError>;

    /// 清理 — 删除 trace_id 下除最近 `keep` 个之外的所有 Checkpoint。
    /// 返回实际删除的数量。
    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError>;
}

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// 增量快照（可选 — 用于减少序列化成本）
    pub snapshot: Option<StateSnapshot>,
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
            snapshot: None,
        }
    }

    /// 创建增量快照 Checkpoint。
    ///
    /// - `base_snapshot` — 上次 checkpoint 的完整 State
    /// - `recent_deltas` — 两次 checkpoint 间的增量
    /// - `current_state` — 当前完整 State（用于 fallback）
    pub fn with_snapshot(
        parent_trace_id: TraceId,
        graph_hash: impl Into<String>,
        current_node: impl Into<String>,
        current_state: State,
        base_snapshot: State,
        recent_deltas: Vec<StateDelta>,
    ) -> Self {
        Self {
            checkpoint_id: CheckpointId(uuid::Uuid::new_v4()),
            parent_trace_id,
            graph_hash: graph_hash.into(),
            current_node: NodeId(current_node.into()),
            state: current_state,
            created_at: chrono_like_timestamp(),
            snapshot: Some(StateSnapshot {
                base_snapshot,
                recent_deltas,
            }),
        }
    }

    /// 恢复 State — 优先使用增量快照，fallback 到完整快照。
    ///
    /// 如果有 snapshot，从 base + apply(deltas) 恢复。
    /// 否则直接使用 state 字段。
    pub fn restore_state(&self) -> State {
        if let Some(snapshot) = &self.snapshot {
            snapshot.restore()
        } else {
            self.state.clone()
        }
    }
}

/// 增量 State 快照 — 减少序列化成本。
///
/// 核心思想：不每次都保存完整 State，
/// 而是保存 base_snapshot + recent_deltas。
/// 恢复时：base + apply(deltas) → 避免频繁全量序列化。
///
/// # 内存节省
///
/// 假设 State 500KB，每次 checkpoint 保存 10 个 delta（每个 1KB）：
/// - 全量：500KB × N 次 = 5MB
/// - 增量：500KB + 10KB × N 次 = 1.5MB（N=10 时节省 70%）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// 上次 checkpoint 的完整 State（base）
    pub base_snapshot: State,
    /// 两次 checkpoint 间的增量（最近的 delta 在最后）
    pub recent_deltas: Vec<StateDelta>,
}

impl StateSnapshot {
    /// 从 base + deltas 恢复完整 State。
    pub fn restore(&self) -> State {
        let mut state = self.base_snapshot.clone();
        // 按顺序 apply deltas（简单的 last-write-wins）
        for delta in &self.recent_deltas {
            match delta.op {
                crate::delta::DeltaOp::Put => {
                    state.insert(delta.key.to_string(), delta.value.clone());
                }
                crate::delta::DeltaOp::Delete => {
                    state.remove(delta.key.as_ref());
                }
            }
        }
        state
    }

    /// 获取 base snapshot 的大小估算（字节）。
    pub fn base_size_bytes(&self) -> usize {
        serde_json::to_vec(&self.base_snapshot)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// 获取 deltas 的大小估算（字节）。
    pub fn deltas_size_bytes(&self) -> usize {
        serde_json::to_vec(&self.recent_deltas)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// 总大小估算（字节）。
    pub fn total_size_bytes(&self) -> usize {
        self.base_size_bytes() + self.deltas_size_bytes()
    }

    /// 压缩 — 如果 deltas 累积过多，重新生成 base。
    ///
    /// 当 `recent_deltas.len() > threshold` 时，
    /// 将当前恢复结果作为新 base，清空 deltas。
    pub fn compact(&mut self, threshold: usize) {
        if self.recent_deltas.len() > threshold {
            let restored = self.restore();
            self.base_snapshot = restored;
            self.recent_deltas.clear();
        }
    }
}

// ─── IncrementalSnapshotState ─────────────────────────────────

/// 增量快照运行时状态 — 在 Executor 的 run_loop 中维护。
///
/// 跟踪上次 checkpoint 的 base 和累积的 deltas，
/// 用于在保存 checkpoint 时生成增量快照。
#[derive(Debug, Clone, Default)]
pub struct IncrementalSnapshotState {
    /// 上次 checkpoint 的完整 State（base）
    pub base_state: Option<State>,
    /// 上次 checkpoint 以来的 deltas
    pub pending_deltas: Vec<StateDelta>,
    /// delta 累积阈值（超过此值重新生成 base）
    pub compact_threshold: usize,
}

impl IncrementalSnapshotState {
    /// 创建新的增量快照状态。
    pub fn new(compact_threshold: usize) -> Self {
        Self {
            base_state: None,
            pending_deltas: Vec::new(),
            compact_threshold,
        }
    }

    /// 记录一个 delta（节点执行后调用）。
    pub fn record_delta(&mut self, delta: StateDelta) {
        self.pending_deltas.push(delta);
    }

    /// 记录多个 delta。
    pub fn record_deltas(&mut self, deltas: Vec<StateDelta>) {
        self.pending_deltas.extend(deltas);
    }

    /// 生成增量快照（保存 checkpoint 时调用）。
    ///
    /// 返回 `(base_state, recent_deltas, current_state)`：
    /// - `base_state` — 上次 checkpoint 的完整 State（如果没有则为 None）
    /// - `recent_deltas` — 上次 checkpoint 以来的 deltas
    /// - `current_state` — 当前完整 State（用于 fallback）
    pub fn snapshot(&mut self, current_state: &State) -> (Option<State>, Vec<StateDelta>, State) {
        let base = self.base_state.clone();
        let deltas = std::mem::take(&mut self.pending_deltas);

        // 压缩：如果 deltas 过多，重新生成 base
        if base.is_some() && deltas.len() > self.compact_threshold {
            // 使用当前 State 作为新 base
            self.base_state = Some(current_state.clone());
            self.pending_deltas.clear();
            return (None, Vec::new(), current_state.clone());
        }

        (base, deltas, current_state.clone())
    }

    /// 从 checkpoint 恢复增量快照状态。
    pub fn from_checkpoint(checkpoint: &Checkpoint) -> Self {
        if let Some(snapshot) = &checkpoint.snapshot {
            Self {
                base_state: Some(snapshot.base_snapshot.clone()),
                pending_deltas: snapshot.recent_deltas.clone(),
                compact_threshold: 20,
            }
        } else {
            // 全量快照 — 将 state 作为 base
            Self {
                base_state: Some(checkpoint.state.clone()),
                pending_deltas: Vec::new(),
                compact_threshold: 20,
            }
        }
    }

    /// 清空 pending deltas（checkpoint 保存成功后调用）。
    pub fn clear_pending(&mut self) {
        self.pending_deltas.clear();
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

/// Trace ID — 从 lellm-core 导入。
pub use lellm_core::TraceId;

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
