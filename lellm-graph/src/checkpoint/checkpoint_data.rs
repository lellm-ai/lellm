//! Checkpoint — 执行恢复的唯一数据源。
//!
//! 分层架构：
//! ```text
//! ExecutionEngine (Trigger)
//!   ↓ on_checkpoint(&state, &frame_info)  [async，同步等待保存完成]
//! CheckpointSink (SPI — 策略层)
//!   ↓ 自行决定
//! CheckpointSaveSink → CheckpointConfig → BlobCheckpointStore（InMemory/File）
//! ```
//!
//! # Trigger / Storage 分离
//!
//! **ExecutionEngine** 负责定义一致的 checkpoint 语义——什么时候产生一个恢复点。
//! 唯一的位置：`execute() → commit() → 路由解析 → checkpoint(next) → 下一节点`。
//!
//! **CheckpointSink** 负责决定是否真的保存、保存到哪里、保存多少。
//! Engine 只管借用 `&dyn CheckpointSink<S>`，不知道存储后端。
//!
//! # 恢复点语义（format_version=1）
//!
//! checkpoint 保存的是**下一个要执行的节点**（`next_node: Option<NodeId>`，
//! None = 已完成）+ 状态快照 + 已执行步数（`steps_used`，恢复时预算延续）。
//! 恢复直接使用已确定的执行位置，不重新解析上一节点的路由。

use std::fmt::Debug;

use serde::{Deserialize, Serialize};

use crate::state::State;
use crate::state::workflow_state::WorkflowState;

// ─── CheckpointId ──────────────────────────────────────────────

/// Checkpoint 格式版本 — 首个带版本格式（旧无版本格式称 legacy，拒绝加载）。
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

/// Checkpoint 唯一标识 — sparkid（21 字符 Base58，时间可排序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub sparkid::SparkId);

impl CheckpointId {
    pub fn new() -> Self {
        Self(sparkid::SparkId::new())
    }
}

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

/// 执行检查点 — 物化快照 + 执行游标（format_version=1）。
///
/// 唯一职责：恢复（Restore）。给我一个 Checkpoint，我从 `next_node` 开始、
/// 用 `state` 与 `steps_used` 预算继续执行；`next_node = None` 表示已完成。
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
///
/// # 严格性说明（重要）
///
/// 本结构体保留普通 `derive(Deserialize)`，**直接反序列化成功不等于「检查点已验证」**：
/// - 存储加载路径（`SerdeCheckpointCodec::deserialize`）执行两段式严格校验
///   （语法 → 结构 → 类型化），legacy 格式/缺 `next_node` 键 → `UnsupportedFormat`；
/// - 恢复入口（`SimpleExecutor::execute_stream_with_restore`）对**直接构造**的
///   检查点再做一次校验（版本/指纹/节点存在/步数边界/最新性）。
/// 两条入口之外的直接 `serde_json::from_str::<Checkpoint<_>>` 不受格式保护。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint<S: WorkflowState = State> {
    /// 格式版本 — 加载时严格校验（缺失或不支持 → 拒绝）
    pub format_version: u32,
    /// 唯一标识
    pub checkpoint_id: CheckpointId,
    /// 下一个要执行的节点；None = 执行已完成
    pub next_node: Option<NodeId>,
    /// 物化状态快照（P0-1: 使用 Checkpoint 关联类型，不是 raw State）
    pub state: S::Checkpoint,
    /// 图结构指纹 — 恢复时校验兼容性
    pub graph_hash: u64,
    /// 已执行步数 — 恢复时步数预算从此延续（max_steps 为总预算）
    pub steps_used: usize,
    /// 创建时间（仅展示用途，不参与排序）
    pub created_at: std::time::SystemTime,
}

impl<S: WorkflowState> Checkpoint<S> {
    /// 创建 Checkpoint（使用 snapshot() 投影）。
    pub fn new(next_node: Option<NodeId>, state: &S, graph_hash: u64, steps_used: usize) -> Self {
        Self {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: CheckpointId::new(),
            next_node,
            state: state.snapshot(),
            graph_hash,
            steps_used,
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
    #[error("unsupported checkpoint format: {0}")]
    UnsupportedFormat(String),
}

// ─── TraceId Re-export ─────────────────────────────────────────

/// 从 ids 模块重导出 TraceId。
///
/// 注意：Checkpoint 结构体**不包含** trace_id。
/// 关联关系由存储层组织（如同一目录下的文件）。
pub use crate::ids::TraceId;

// ─── FrameInfo ─────────────────────────────────────────────────

/// Checkpoint 边界描述 — Engine 传递给 Sink 的最小上下文。
///
/// 设计原则：极简。Engine 只传递"我到了哪里"，Sink 自行决定
/// 是否记录、如何记录、记录多少。
///
/// - 想做节流？Sink 自己维护计数器。
/// - 想做脏检查？Sink 自己缓存上次 snapshot 的 hash。
/// - 想过滤特定节点？Sink 匹配 `next_node`。
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// 下一个要执行的节点（None = 已完成）
    pub next_node: Option<NodeId>,
    /// 已执行步数（从运行入口累计；恢复时从 steps_used 延续）
    pub step: usize,
}

impl FrameInfo {
    /// 创建 FrameInfo。
    pub fn new(next_node: Option<NodeId>, step: usize) -> Self {
        Self { next_node, step }
    }
}

// ─── CheckpointSink ────────────────────────────────────────────

/// Checkpoint Sink SPI — 执行引擎通知 Sink 到达了合法的恢复边界。
///
/// Engine 保证：
/// - 每次调用时，State 已 commit（mutation 已 apply），状态是一致的。
/// - 调用顺序：`execute() → commit() → on_checkpoint() → route()`。
///
/// Sink 自行决定：
/// - 是否记录（节流、过滤）
/// - 是否 snapshot（借用 `&S`，Sink 决定是否 clone）
/// - 序列化格式（serde、protobuf、binary）
/// - 存储后端（内存、磁盘、网络）
///
/// # 设计原则
///
/// Engine 不拥有 Checkpoint 生命周期，只借用 Sink。
///
/// # 同步语义
///
/// `on_checkpoint` 是 async 且**必须等待保存完成**：保存失败返回 `Err`，
/// 执行在该边界停止（不越过边界继续执行下一节点）。
#[async_trait::async_trait]
pub trait CheckpointSink<S: WorkflowState>: Send + Sync {
    /// 到达恢复边界（State 已 commit，next 已确定）。
    ///
    /// `state` 是借用——Sink 决定是否 snapshot/clone。
    /// `frame` 描述当前执行位置（next_node + step）。
    async fn on_checkpoint(
        &mut self,
        state: &S,
        frame: &FrameInfo,
    ) -> Result<(), CheckpointStoreError>;
}

/// 空 Sink — 不记录任何内容。
///
/// 用于不需要 Checkpoint 的场景（如 ToolUseLoop 的简单调用）。
#[derive(Debug, Default)]
pub struct NoopCheckpointSink;

#[async_trait::async_trait]
impl<S: WorkflowState> CheckpointSink<S> for NoopCheckpointSink {
    async fn on_checkpoint(
        &mut self,
        _state: &S,
        _frame: &FrameInfo,
    ) -> Result<(), CheckpointStoreError> {
        // 什么都不做
        Ok(())
    }
}

// ─── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateMerge;
    use crate::{GraphBuilder, NodeKind, TaskNode};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn test_noop_checkpoint_sink() {
        // 验证 NoopCheckpointSink 不记录任何内容
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.end("a");
        let graph = Arc::new(builder.build().unwrap());

        let mut sink = NoopCheckpointSink;
        let mut state = State::new();
        let mut engine: crate::ExecutionEngine<'_, State> = crate::ExecutionEngine::new(
            &mut state,
            None,
            CancellationToken::new(),
            Some(&mut sink),
            None,
        );

        let mut cb = crate::graph::NoopStepCallback;
        graph.run_inline(&mut engine, 100, &mut cb).await.unwrap();
        // NoopSink 不记录，无需断言
    }

    #[test]
    fn test_frame_info_minimal() {
        let info = FrameInfo::new(Some(NodeId("test_node".into())), 42);
        assert_eq!(info.next_node, Some(NodeId("test_node".into())));
        assert_eq!(info.step, 42);
    }
}
