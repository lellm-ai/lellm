//! Checkpoint — 执行恢复的唯一数据源。
//!
//! 分层架构：
//! ```text
//! ExecutionEngine (Trigger)
//!   ↓ on_checkpoint(&state, &frame_info)
//! CheckpointSink (SPI — 策略层)
//!   ↓ 自行决定
//! MemorySink → FrameStack (内存)
//! DiskSink   → 每 N 步 snapshot → 磁盘
//! NetworkSink → protobuf → remote
//! ```
//!
//! # Trigger / Storage 分离
//!
//! **ExecutionEngine** 负责定义一致的 checkpoint 语义——什么时候产生一个恢复点。
//! 唯一的位置：`execute() → commit() → checkpoint() → route()`。
//!
//! **CheckpointSink** 负责决定是否真的保存、保存到哪里、保存多少。
//! Engine 只管借用 `&dyn CheckpointSink<S>`，不知道 FrameStack、磁盘、网络。
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
use crate::state::workflow_state::WorkflowState;

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
// ─── Phase 6: Execution Frame Snapshot ────────────────────────
/// 执行帧 — 保存单个 Graph 的执行位置。
///
/// 可序列化 — 用于 SessionCheckpoint 持久化。
#[derive(Clone, Serialize, Deserialize)]
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

impl<S: WorkflowState> Debug for Frame<S>
where
    S::Checkpoint: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("graph_id", &self.graph_id)
            .field("node_id", &self.node_id)
            .field("state", &self.state)
            .field("cursor", &self.cursor)
            .finish()
    }
}

/// 帧栈 — 保存完整的执行位置历史。
///
/// 可序列化 — 用于 SessionCheckpoint 持久化。
#[derive(Clone, Serialize, Deserialize)]
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

impl<S: WorkflowState> Debug for FrameStack<S>
where
    S::Checkpoint: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameStack")
            .field("frames", &self.frames)
            .finish()
    }
}

// ─── FrameInfo ─────────────────────────────────────────────────

/// Checkpoint 边界描述 — Engine 传递给 Sink 的最小上下文。
///
/// 设计原则：极简。Engine 只传递"我到了哪里"，Sink 自行决定
/// 是否记录、如何记录、记录多少。
///
/// - 想做节流？Sink 自己维护计数器。
/// - 想做脏检查？Sink 自己缓存上次 snapshot 的 hash。
/// - 想过滤特定节点？Sink 匹配 `node_id`。
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// 当前节点 ID（commit 刚完成的节点）
    pub node_id: String,
    /// 执行步数（从 run_inline 入口开始计数）
    pub step: usize,
}

impl FrameInfo {
    /// 创建 FrameInfo。
    pub fn new(node_id: impl Into<String>, step: usize) -> Self {
        Self {
            node_id: node_id.into(),
            step,
        }
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
/// 这与 D6 原则一致——Engine 不知道 FrameStack。
pub trait CheckpointSink<S: WorkflowState>: Send + Sync {
    /// 节点完成，State 已 commit。
    ///
    /// `state` 是借用——Sink 决定是否 snapshot/clone。
    /// `frame` 描述当前执行位置。
    fn on_checkpoint(&mut self, state: &S, frame: &FrameInfo);
}

/// 空 Sink — 不记录任何内容。
///
/// 用于不需要 Checkpoint 的场景（如 ToolUseLoop 的简单调用）。
#[derive(Debug, Default)]
pub struct NoopCheckpointSink;

impl<S: WorkflowState> CheckpointSink<S> for NoopCheckpointSink {
    fn on_checkpoint(&mut self, _state: &S, _frame: &FrameInfo) {
        // 什么都不做
    }
}

/// 内存 Sink — 将所有 checkpoint 记录到内存。
///
/// 用于调试、测试、time travel。
///
/// 要求 `S::Checkpoint: Debug`（Frame 需要 Debug）。
pub struct MemorySink<S: WorkflowState = State> {
    pub frames: Vec<Frame<S>>,
}

impl<S: WorkflowState> Debug for MemorySink<S>
where
    S::Checkpoint: Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySink")
            .field("frames", &self.frames)
            .finish()
    }
}

impl<S: WorkflowState> Default for MemorySink<S> {
    fn default() -> Self {
        Self { frames: Vec::new() }
    }
}

impl<S: WorkflowState> MemorySink<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_frames(self) -> Vec<Frame<S>> {
        self.frames
    }
}

impl<S: WorkflowState> CheckpointSink<S> for MemorySink<S>
where
    S::Checkpoint: Sync,
{
    fn on_checkpoint(&mut self, state: &S, frame: &FrameInfo) {
        self.frames.push(Frame {
            graph_id: String::new(), // Engine 不传递 graph_id，由 Sink 填充
            node_id: frame.node_id.clone(),
            state: state.snapshot(),
            cursor: frame.step,
        });
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
    async fn test_auto_checkpoint_via_memory_sink() {
        // 创建一个简单的 Graph: start → a → b → end
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        builder.end("b");
        builder.edge("a", "b");
        let graph = Arc::new(builder.build().unwrap());

        // 创建 MemorySink
        let mut sink = MemorySink::<State>::new();

        // 创建 Engine 并绑定 sink
        let mut state = State::new();
        let mut engine: crate::ExecutionEngine<'_, State> = crate::ExecutionEngine::new(
            &mut state,
            None,
            CancellationToken::new(),
            Some(&mut sink),
            None,
        );

        // 执行
        let mut cb = crate::graph::NoopStepCallback;
        graph.run_inline(&mut engine, 100, &mut cb).await.unwrap();

        // 验证：应该有 2 个 checkpoint（a 和 b）
        assert_eq!(sink.frames.len(), 2);
        assert_eq!(sink.frames[0].node_id, "a");
        assert_eq!(sink.frames[1].node_id, "b");
        assert_eq!(sink.frames[0].cursor, 1);
        assert_eq!(sink.frames[1].cursor, 2);
    }

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
        let info = FrameInfo::new("test_node", 42);
        assert_eq!(info.node_id, "test_node");
        assert_eq!(info.step, 42);
    }
}
