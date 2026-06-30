//! Checkpoint — 执行恢复的唯一数据源。
//!
//! 分层架构：
//! ```text
//! Checkpoint<S>           ← Workflow 层，强类型，纯 Snapshot 模型
//!        │
//!        ▼ serialize/deserialize
//! CheckpointCodec<S>      ← 序列化层，对象 ↔ 二进制表示
//!        │
//!        ▼
//! CheckpointBlob           ← 跨 Codec 的统一载体
//!        │
//!        ▼ save/load
//! BlobCheckpointStore      ← 存储层 SPI，bytes in / bytes out
//!        │
//!        ▼
//! Memory / File / S3 / SQLite  ← 后端实现
//! ```
//!
//! # Phase 6: Execution Frame Snapshot
//!
//! 核心洞察：checkpoint 不是保存 state，而是保存 execution position + state projection。
//!
//! ```text
//! checkpoint 的边界单位是 Graph Execution Frame，不是 WorkflowState 或 Node。
//!
//! 正确模型：
//!   Graph Execution = Frame Stack
//!
//! Frame = {
//!     graph_id,
//!     node_id,
//!     state_snapshot,
//!     cursor,
//! }
//!
//! checkpoint = FrameStack snapshot
//! ```

use std::fmt::Debug;

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
///
/// # P0-1: Checkpoint Projection
///
/// `state` 字段使用 `S::Checkpoint`（关联类型），不是 `S`（Runtime State）。
/// 这保证：
/// - Runtime State 可以包含不可序列化字段（`Arc<dyn ...>`, `Sender`, `Cache`）
/// - Checkpoint 只序列化必要字段
/// - 编译期保证可序列化
///
/// # Graph Compatibility
///
/// `graph_hash` 记录创建 Checkpoint 时的图结构指纹。
/// 恢复时必须校验：`graph_hash` 不匹配 → 拒绝恢复（不允许 silent mismatch）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint<S: WorkflowState = State> {
    /// 唯一标识
    pub checkpoint_id: CheckpointId,
    /// 下一个要执行的节点
    pub current_node: NodeId,
    /// 物化状态快照（P0-1: 使用 Checkpoint 关联类型，不是 raw State）
    pub state: S::Checkpoint,
    /// 图结构指纹 — 恢复时校验兼容性
    pub graph_hash: u64,
    /// 创建时间
    pub created_at: std::time::SystemTime,
}

impl<S: WorkflowState> Checkpoint<S> {
    /// 从 Runtime State 创建 Checkpoint（使用 snapshot() 投影）。
    pub fn new(current_node: impl Into<String>, state: &S, graph_hash: u64) -> Self {
        Self {
            checkpoint_id: CheckpointId(uuid::Uuid::new_v4()),
            current_node: NodeId(current_node.into()),
            state: state.snapshot(),
            graph_hash,
            created_at: std::time::SystemTime::now(),
        }
    }

    /// 从 Checkpoint 恢复 Runtime State（使用 restore()）。
    pub fn restore_state(self) -> S {
        S::restore(self.state)
    }
}

// ─── CheckpointBlob ────────────────────────────────────────────

/// 跨 Codec 的统一载体 — 存储层操作的对象。
///
/// 将序列化后的二进制数据与元数据打包，供 CheckpointStore 使用。
/// 存储层无需知道 State 类型或序列化格式。
///
/// `graph_hash` 作为 correctness invariant 存储：
/// 恢复时校验 `graph_hash` 不匹配 → reject，不允许 silent mismatch。
#[derive(Debug, Clone)]
pub struct CheckpointBlob {
    /// Checkpoint 唯一标识
    pub id: CheckpointId,
    /// 序列化后的二进制数据（格式由 Codec 决定）
    pub data: Vec<u8>,
    /// 图结构指纹 — 恢复时校验兼容性
    pub graph_hash: u64,
    /// 创建时间
    pub created_at: std::time::SystemTime,
}

impl CheckpointBlob {
    pub fn new(
        id: CheckpointId,
        data: Vec<u8>,
        graph_hash: u64,
        created_at: std::time::SystemTime,
    ) -> Self {
        Self {
            id,
            data,
            graph_hash,
            created_at,
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
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("graph mismatch: expected hash {expected:#018x}, got {actual:#018x}")]
    GraphMismatch { expected: u64, actual: u64 },
}

// ─── TraceId Re-export ─────────────────────────────────────────

/// 从 ids 模块重导出 TraceId。
///
/// 注意：Checkpoint 结构体**不包含** trace_id。
/// 关联关系由存储层组织（如同一目录下的文件）。
pub use crate::ids::TraceId;

// ─── CheckpointPolicy 已迁移 ──────────────────────────────────

/// 向后兼容 — CheckpointPolicy 已迁移至 checkpoint_policy 模块。
/// v0.5 使用 TriggerPolicy + RetentionPolicy 替代。
#[allow(deprecated)]
#[doc(inline)]
pub use crate::checkpoint_policy::CheckpointPolicy;

// ─── Phase 6: Execution Frame Snapshot ────────────────────────

/// 执行帧 — 保存单个 Graph 的执行位置。
#[derive(Debug, Clone)]
pub struct Frame<S: WorkflowState = State> {
    /// 图 ID
    pub graph_id: String,

    /// 当前节点 ID
    pub node_id: String,

    /// 状态快照（P0-1: 使用 Checkpoint 关联类型，可序列化）
    pub state: S::Checkpoint,

    /// 执行游标（节点索引或步骤数）
    pub cursor: usize,
}

impl<S: WorkflowState> Frame<S> {
    /// 从 Runtime State 创建 Frame（使用 snapshot() 投影）。
    pub fn new(graph_id: String, node_id: String, state: &S, cursor: usize) -> Self {
        Self {
            graph_id,
            node_id,
            state: state.snapshot(),
            cursor,
        }
    }
}

/// 帧栈 — 保存完整的执行位置历史。
#[derive(Debug, Clone)]
pub struct FrameStack<S: WorkflowState = State>
where
    S::Checkpoint: Debug,
{
    /// 帧列表（从外到内）
    frames: Vec<Frame<S>>,
}

impl<S: WorkflowState> FrameStack<S>
where
    S::Checkpoint: Debug,
{
    /// 创建空的帧栈。
    pub fn new() -> Self {
        Self { frames: Vec::new() }
    }

    /// Push 一个新帧。
    pub fn push(&mut self, frame: Frame<S>) {
        self.frames.push(frame);
    }

    /// Pop 最后一个帧。
    pub fn pop(&mut self) -> Option<Frame<S>> {
        self.frames.pop()
    }

    /// 获取当前帧（最顶层）。
    pub fn current(&self) -> Option<&Frame<S>> {
        self.frames.last()
    }

    /// 获取帧数量。
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// 检查是否为空。
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// 获取所有帧的引用。
    pub fn frames(&self) -> &[Frame<S>] {
        &self.frames
    }
}

impl<S: WorkflowState> Default for FrameStack<S>
where
    S::Checkpoint: Debug,
{
    fn default() -> Self {
        Self::new()
    }
}
