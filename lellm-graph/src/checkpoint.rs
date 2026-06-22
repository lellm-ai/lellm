//! Checkpoint — 执行恢复的唯一数据源。
//!
//! Checkpoint 的唯一职责：**恢复（Restore）**。
//!
//! - 不含 `parent_trace_id` — 与 Trace 通过存储层组织关联，非结构体关联
//! - 不含 `effect_log` — Effect 审计走 `ExecutionTrace`
//! - 不含 `snapshot` — 增量快照是存储层优化，不应泄漏到 Checkpoint 结构
//!
//! 给我一个 Checkpoint 文件，我就能恢复。不需要任何其他东西。

use serde::{Deserialize, Serialize};

use crate::state::State;
use crate::workflow_state::WorkflowState;

// ─── CheckpointId ──────────────────────────────────────────────

/// Checkpoint 唯一标识。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub uuid::Uuid);

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── NodeId ────────────────────────────────────────────────────

/// 节点标识。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── Checkpoint ────────────────────────────────────────────────

/// 执行检查点 — 物化快照 + 执行游标。
///
/// Checkpoint 的唯一职责：恢复（Restore）。
/// 给我一个 Checkpoint，我就能从 `current_node` 开始，用 `state` 继续执行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint<S = State> {
    /// 唯一标识
    pub checkpoint_id: CheckpointId,
    /// 下一个要执行的节点
    pub current_node: NodeId,
    /// 物化状态快照
    pub state: S,
    /// 创建时间
    pub created_at: std::time::SystemTime,
}

impl<S: WorkflowState> Checkpoint<S> {
    pub fn new(current_node: impl Into<String>, state: S) -> Self {
        Self {
            checkpoint_id: CheckpointId(uuid::Uuid::new_v4()),
            current_node: NodeId(current_node.into()),
            state,
            created_at: std::time::SystemTime::now(),
        }
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
///
/// 与类型解耦 — 存储层序列化/反序列化 `S`。
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    /// 保存 Checkpoint 并关联 trace_id。
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        checkpoint: &Checkpoint,
    ) -> Result<(), CheckpointStoreError>;
    async fn load(&self, id: &CheckpointId) -> Result<Option<Checkpoint>, CheckpointStoreError>;
    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<Checkpoint>, CheckpointStoreError>;
    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError>;
    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError>;
    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError>;
}

// ─── CheckpointPolicy ──────────────────────────────────────────

/// Checkpoint 策略 — 控制何时保存。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckpointPolicy {
    /// 每次节点执行后保存
    #[default]
    EveryNode,
    /// 仅在 Barrier 决策后保存
    BarrierOnly,
    /// 手动控制 — 调用方显式触发
    Manual,
}

// ─── TraceId Re-export ─────────────────────────────────────────

/// 从 ids 模块重导出 TraceId，供 CheckpointStore trait 使用。
///
/// 注意：Checkpoint 结构体**不包含** trace_id。
/// 关联关系由存储层组织（如同一目录下的文件）。
pub use crate::ids::TraceId;
