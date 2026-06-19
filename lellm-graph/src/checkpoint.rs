//! Checkpoint + ExecutionTrace — 从 lellm-runtime 合并。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::delta::{DeltaOp, ReducerRegistry, StateDelta};
use crate::ids::TraceId;
use crate::state::State;

// ─── CheckpointPolicy ──────────────────────────────────────────

/// Checkpoint 触发时机。
#[derive(Debug, Clone)]
pub enum CheckpointTrigger {
    BarrierResolved,
    ExecutionCompleted,
    HumanDecision,
    Explicit,
    Adaptive(ExecutionMetadata),
}

impl PartialEq for CheckpointTrigger {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::BarrierResolved, Self::BarrierResolved)
                | (Self::ExecutionCompleted, Self::ExecutionCompleted)
                | (Self::HumanDecision, Self::HumanDecision)
                | (Self::Explicit, Self::Explicit)
                | (Self::Adaptive(_), Self::Adaptive(_))
        )
    }
}

/// Checkpoint 策略。
#[derive(Debug, Clone)]
pub struct CheckpointPolicy {
    pub triggers: Vec<CheckpointTrigger>,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self::conservative()
    }
}

impl CheckpointPolicy {
    pub fn conservative() -> Self {
        Self {
            triggers: vec![
                CheckpointTrigger::BarrierResolved,
                CheckpointTrigger::ExecutionCompleted,
                CheckpointTrigger::HumanDecision,
            ],
        }
    }

    pub fn minimal() -> Self {
        Self {
            triggers: vec![
                CheckpointTrigger::BarrierResolved,
                CheckpointTrigger::ExecutionCompleted,
            ],
        }
    }

    pub fn manual() -> Self {
        Self {
            triggers: vec![CheckpointTrigger::Explicit],
        }
    }

    pub fn should_checkpoint_on_barrier(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::BarrierResolved)
    }

    pub fn should_checkpoint_on_completion(&self) -> bool {
        self.triggers
            .contains(&CheckpointTrigger::ExecutionCompleted)
    }

    pub fn should_checkpoint_on_human_decision(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::HumanDecision)
    }

    pub fn should_checkpoint_on_explicit(&self) -> bool {
        self.triggers.contains(&CheckpointTrigger::Explicit)
    }

    pub fn has_adaptive_trigger(&self) -> bool {
        self.triggers
            .iter()
            .any(|t| matches!(t, CheckpointTrigger::Adaptive(_)))
    }
}

// ─── ExecutionMetadata ────────────────────────────────────────

/// 节点执行元数据 — 用于 Adaptive Checkpoint 决策。
#[derive(Debug, Clone, Default)]
pub struct ExecutionMetadata {
    pub duration_ms: u64,
    pub token_cost: f64,
    pub has_side_effects: bool,
}

impl ExecutionMetadata {
    pub fn lightweight() -> Self {
        Self {
            duration_ms: 2,
            token_cost: 0.0,
            has_side_effects: false,
        }
    }

    pub fn heavy() -> Self {
        Self {
            duration_ms: 90_000,
            token_cost: 0.01,
            has_side_effects: false,
        }
    }

    pub fn with_side_effects() -> Self {
        Self {
            duration_ms: 0,
            token_cost: 0.0,
            has_side_effects: true,
        }
    }
}

/// Checkpoint 评分。
#[derive(Debug, Clone)]
pub struct CheckpointScore {
    pub duration_weight: f64,
    pub token_weight: f64,
    pub side_effect_weight: f64,
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
    pub fn calculate(&self, metadata: &ExecutionMetadata) -> f64 {
        let mut score = self.duration_weight * metadata.duration_ms as f64;
        score += self.token_weight * metadata.token_cost;
        if metadata.has_side_effects {
            score += self.side_effect_weight;
        }
        score
    }

    pub fn should_checkpoint(&self, metadata: &ExecutionMetadata) -> bool {
        self.calculate(metadata) >= self.threshold
    }
}

// ─── CheckpointStoreError ──────────────────────────────────────

/// Checkpoint 存储操作错误。
#[derive(Debug, thiserror::Error)]
pub enum CheckpointStoreError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("checkpoint not found: {0}")]
    NotFound(CheckpointId),
    #[error("corrupted checkpoint: {0}")]
    Corrupted(String),
}

// ─── CheckpointStore trait ─────────────────────────────────────

/// Checkpoint 存储后端 SPI。
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointStoreError>;
    async fn load(&self, id: &CheckpointId) -> Result<Option<Checkpoint>, CheckpointStoreError>;
    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<Checkpoint>, CheckpointStoreError>;
    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError>;
    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError>;
    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError>;
}

// ─── Checkpoint ─────────────────────────────────────────────────

/// Checkpoint ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub uuid::Uuid);

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 执行游标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Checkpoint — Materialized State + Execution Cursor。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub checkpoint_id: CheckpointId,
    pub parent_trace_id: TraceId,
    pub graph_hash: String,
    pub current_node: NodeId,
    pub state: State,
    pub created_at: String,
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

    pub fn restore_state(
        &self,
        registry: &ReducerRegistry,
    ) -> Result<State, crate::state::StateError> {
        if let Some(snapshot) = &self.snapshot {
            snapshot.restore(registry)
        } else {
            Ok(self.state.clone())
        }
    }

    pub fn restore_state_simple(&self) -> State {
        if let Some(snapshot) = &self.snapshot {
            snapshot.restore_simple()
        } else {
            self.state.clone()
        }
    }
}

/// 增量 State 快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub base_snapshot: State,
    pub recent_deltas: Vec<StateDelta>,
}

impl StateSnapshot {
    pub fn restore(&self, registry: &ReducerRegistry) -> Result<State, crate::state::StateError> {
        let mut state = self.base_snapshot.clone();
        registry.merge_deltas(&mut state, &self.recent_deltas)?;
        Ok(state)
    }

    pub fn restore_simple(&self) -> State {
        let mut state = self.base_snapshot.clone();
        for delta in &self.recent_deltas {
            match delta.op {
                DeltaOp::Put => {
                    state.insert(delta.key.to_string(), delta.value.clone());
                }
                DeltaOp::Delete => {
                    state.remove(delta.key.as_ref());
                }
            }
        }
        state
    }

    pub fn base_size_bytes(&self) -> usize {
        serde_json::to_vec(&self.base_snapshot)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    pub fn deltas_size_bytes(&self) -> usize {
        serde_json::to_vec(&self.recent_deltas)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    pub fn total_size_bytes(&self) -> usize {
        self.base_size_bytes() + self.deltas_size_bytes()
    }

    pub fn compact(&mut self, threshold: usize) {
        if self.recent_deltas.len() > threshold {
            let restored = self.restore_simple();
            self.base_snapshot = restored;
            self.recent_deltas.clear();
        }
    }
}

// ─── IncrementalSnapshotState ─────────────────────────────────

/// 增量快照运行时状态。
#[derive(Debug, Clone, Default)]
pub struct IncrementalSnapshotState {
    pub base_state: Option<State>,
    pub pending_deltas: Vec<StateDelta>,
    pub compact_threshold: usize,
}

impl IncrementalSnapshotState {
    pub fn new(compact_threshold: usize) -> Self {
        Self {
            base_state: None,
            pending_deltas: Vec::new(),
            compact_threshold,
        }
    }

    pub fn record_delta(&mut self, delta: StateDelta) {
        self.pending_deltas.push(delta);
    }

    pub fn record_deltas(&mut self, deltas: Vec<StateDelta>) {
        self.pending_deltas.extend(deltas);
    }

    pub fn snapshot(&mut self, current_state: &State) -> (Option<State>, Vec<StateDelta>, State) {
        let base = self.base_state.clone();
        let deltas = std::mem::take(&mut self.pending_deltas);

        if base.is_some() && deltas.len() > self.compact_threshold {
            self.base_state = Some(current_state.clone());
            self.pending_deltas.clear();
            return (None, Vec::new(), current_state.clone());
        }

        (base, deltas, current_state.clone())
    }

    pub fn from_checkpoint(checkpoint: &Checkpoint) -> Self {
        if let Some(snapshot) = &checkpoint.snapshot {
            Self {
                base_state: Some(snapshot.base_snapshot.clone()),
                pending_deltas: snapshot.recent_deltas.clone(),
                compact_threshold: 20,
            }
        } else {
            Self {
                base_state: Some(checkpoint.state.clone()),
                pending_deltas: Vec::new(),
                compact_threshold: 20,
            }
        }
    }

    pub fn clear_pending(&mut self) {
        self.pending_deltas.clear();
    }
}

/// 图变更校验模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphHashMode {
    Strict,
    Force,
}

// ─── ExecutionTrace ─────────────────────────────────────────────

/// 节点执行记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEntry {
    pub step: usize,
    pub node_name: String,
    pub start_time: String,
    pub end_time: String,
    pub success: bool,
    pub error: Option<String>,
}

/// Barrier 决策记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BarrierDecisionRecord {
    pub barrier_id: String,
    pub node_id: String,
    pub decision: Value,
    pub decided_at: String,
}

/// ExecutionTrace — Delta 历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    pub trace_id: TraceId,
    pub initial_state: State,
    pub entries: Vec<ExecutionEntry>,
    pub deltas: Vec<StateDelta>,
    pub barrier_decisions: Vec<BarrierDecisionRecord>,
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
    pub trace_id: TraceId,
    pub state: State,
    pub execution_log: Vec<ExecutionEntry>,
    pub duration_ms: u128,
}

// ─── Helpers ────────────────────────────────────────────────────

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
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
