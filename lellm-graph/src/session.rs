//! ExecutionSession — 执行会话，持有 FrameStack，管理恢复。
//!
//! # 设计理念
//!
//! ```text
//! ExecutionEngine — 一次执行，借用 State（生命周期短）
//! ExecutionSession — 持有 FrameStack，管理恢复（生命周期长）
//!
//! 职责分离：
//! - Engine: 执行逻辑，借用 State
//! - Session: 状态所有权 + FrameStack + Checkpoint 管理
//! ```
//!
//! # P0-1: Checkpoint Projection
//!
//! SessionCheckpoint 使用 `S::Checkpoint`（关联类型），不是 `S`（Runtime State）。
//! 这保证 Runtime State 可以包含不可序列化字段。
//!
//! # P0-2: Graph Hash
//!
//! SessionCheckpoint 使用 `canonical_hash`（从 DSL 层计算），
//! 不依赖 compiled graph 的 HashMap 迭代顺序。

use std::fmt::Debug;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::checkpoint::{CheckpointSink, Frame, FrameInfo, FrameStack};
use crate::graph::Graph;
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};

// ─── SessionError ──────────────────────────────────────────────

/// Session 操作错误。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// Graph Hash 不匹配 — Checkpoint 与当前 Graph 不兼容
    #[error("graph hash mismatch: expected {expected:#018x}, got {actual:#018x}")]
    GraphMismatch { expected: u64, actual: u64 },
}

impl PartialEq for SessionError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                SessionError::GraphMismatch {
                    expected: e1,
                    actual: a1,
                },
                SessionError::GraphMismatch {
                    expected: e2,
                    actual: a2,
                },
            ) => e1 == e2 && a1 == a2,
        }
    }
}

// ─── SessionCheckpointSink ─────────────────────────────────────

/// Session 的 Checkpoint Sink — 将 checkpoint 事件写入 FrameStack。
///
/// 这是 CheckpointSink SPI 的实现之一。Engine 通过借用 `&dyn CheckpointSink<S>`
/// 通知到达恢复边界，SessionCheckpointSink 负责将 Frame 推入 FrameStack。
///
/// # 设计原则
///
/// Engine 不知道 FrameStack 的存在，只调用 `sink.on_checkpoint(&state, &frame_info)`。
/// SessionCheckpointSink 是适配器，将通用的 checkpoint 事件转换为 FrameStack 操作。
pub struct SessionCheckpointSink<'a, S: WorkflowState = State>
where
    S::Checkpoint: Debug,
{
    frame_stack: &'a mut FrameStack<S>,
    graph_name: String,
}

impl<'a, S: WorkflowState> SessionCheckpointSink<'a, S>
where
    S::Checkpoint: Debug,
{
    /// 创建 SessionCheckpointSink，绑定到 FrameStack。
    pub fn new(frame_stack: &'a mut FrameStack<S>, graph_name: impl Into<String>) -> Self {
        Self {
            frame_stack,
            graph_name: graph_name.into(),
        }
    }
}

impl<S: WorkflowState> CheckpointSink<S> for SessionCheckpointSink<'_, S>
where
    S::Checkpoint: Debug + Sync,
{
    fn on_checkpoint(&mut self, state: &S, frame: &FrameInfo) {
        self.frame_stack.push(Frame {
            graph_id: self.graph_name.clone(),
            node_id: frame.node_id.clone(),
            state: state.snapshot(),
            cursor: frame.step,
        });
    }
}

// ─── SessionCheckpoint ─────────────────────────────────────────

/// 会话检查点 — 完整恢复快照。
///
/// 包含：
/// - 状态投影（P0-1: `S::Checkpoint`）
/// - FrameStack（执行位置历史）
/// - graph_hash（P0-2: canonical hash）
///
/// 可序列化 — 用于持久化到文件/数据库。
///
/// # 与 Checkpoint 的区别
///
/// - `Checkpoint<S>` — 单个 Graph 的检查点（current_node + state）
/// - `SessionCheckpoint<S>` — 完整会话的检查点（state + frames + graph_hash）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCheckpoint<S: WorkflowState = State>
where
    S::Checkpoint: Debug,
{
    /// 物化状态快照（P0-1: 使用 Checkpoint 关联类型）
    pub state: S::Checkpoint,
    /// 执行帧栈（完整执行位置历史）
    pub frames: FrameStack<S>,
    /// 图结构指纹（P0-2: canonical hash）
    pub graph_hash: u64,
}

// ─── ExecutionSession ──────────────────────────────────────────

/// 执行会话 — 持有 State 所有权 + FrameStack + Graph 引用。
///
/// # 职责
///
/// - 持有 State 所有权（Engine 只是借用）
/// - 管理 FrameStack（Subgraph 执行时 push/pop）
/// - 创建和恢复 SessionCheckpoint
///
/// # 设计原则
///
/// Graph 是 Immutable 的，多个 Session 共享同一个 Graph 实例。
/// Session 不拥有 Graph，只持有 `Arc<Graph>` 引用。
///
/// ```text
/// Runtime
/// └── Arc<Graph>
///
/// Session1 ──┐
/// Session2 ──┼── Arc<Graph>
/// Session3 ──┘
/// ```
pub struct ExecutionSession<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge>
where
    S::Checkpoint: Debug,
{
    /// 运行时状态（拥有所有权）
    state: S,
    /// 执行帧栈（Subgraph 执行时 push/pop）
    frame_stack: FrameStack<S>,
    /// 图结构（共享引用）
    graph: Arc<Graph<S, M>>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> ExecutionSession<S, M>
where
    S::Checkpoint: Debug,
{
    /// 创建新的执行会话。
    pub fn new(state: S, graph: Arc<Graph<S, M>>) -> Self {
        Self {
            state,
            frame_stack: FrameStack::new(),
            graph,
        }
    }

    /// 从 Checkpoint 恢复。
    ///
    /// # P0-1: 使用 restore() 恢复 State
    ///
    /// `S::restore(checkpoint.state)` 从 checkpoint snapshot 恢复完整 Runtime State。
    ///
    /// # Graph 参数
    ///
    /// 调用方负责提供 `Arc<Graph>`（从 Runtime 获取），
    /// Session 不负责存储或查找 Graph。
    ///
    /// # 错误
    ///
    /// 如果 `checkpoint.graph_hash` 与 `graph.canonical_hash()` 不匹配，
    /// 返回 `SessionError::GraphMismatch`，拒绝恢复。
    pub fn restore(
        checkpoint: SessionCheckpoint<S>,
        graph: Arc<Graph<S, M>>,
    ) -> Result<Self, SessionError> {
        // P0-2: 校验 graph_hash — 不匹配则拒绝恢复
        if checkpoint.graph_hash != graph.canonical_hash() {
            return Err(SessionError::GraphMismatch {
                expected: checkpoint.graph_hash,
                actual: graph.canonical_hash(),
            });
        }

        let state = S::restore(checkpoint.state);
        Ok(Self {
            state,
            frame_stack: checkpoint.frames,
            graph,
        })
    }

    /// 创建 checkpoint — 保存当前执行位置 + 状态投影。
    ///
    /// # P0-1: 使用 snapshot() 进行投影
    ///
    /// `state.snapshot()` 返回 `S::Checkpoint`，只序列化必要字段。
    ///
    /// # P0-2: 使用 canonical_hash
    ///
    /// `graph.canonical_hash()` 从 DSL 层计算，不依赖 HashMap 顺序。
    pub fn checkpoint(&self) -> SessionCheckpoint<S> {
        SessionCheckpoint {
            state: self.state.snapshot(),
            frames: self.frame_stack.clone(),
            graph_hash: self.graph.canonical_hash(),
        }
    }

    /// 获取状态引用。
    pub fn state(&self) -> &S {
        &self.state
    }

    /// 获取状态可变引用。
    pub fn state_mut(&mut self) -> &mut S {
        &mut self.state
    }

    /// 获取帧栈引用。
    pub fn frame_stack(&self) -> &FrameStack<S> {
        &self.frame_stack
    }

    /// 获取帧栈可变引用（用于 Subgraph 执行时 push/pop）。
    pub fn frame_stack_mut(&mut self) -> &mut FrameStack<S> {
        &mut self.frame_stack
    }

    /// 获取图引用。
    pub fn graph(&self) -> &Graph<S, M> {
        &self.graph
    }

    /// 获取图的 Arc 引用（用于共享）。
    pub fn graph_arc(&self) -> Arc<Graph<S, M>> {
        self.graph.clone()
    }

    /// 消费会话，返回最终状态。
    pub fn into_state(self) -> S {
        self.state
    }

    /// 消费会话，返回 (状态, 帧栈)。
    pub fn into_parts(self) -> (S, FrameStack<S>) {
        (self.state, self.frame_stack)
    }
}

impl<S: WorkflowState, M: MergeStrategy<S>> ExecutionSession<S, M>
where
    S::Checkpoint: Debug,
{
    /// 使用指定的 Engine 执行。
    ///
    /// Session 不知道 Stream，Engine 才知道 Stream。
    /// 职责分离：Session 负责 state + frame_stack，Engine 负责执行 + stream。
    ///
    /// # 示例
    ///
    /// ```ignore
    /// // 创建 Checkpoint Sink（可选）
    /// let mut sink = SessionCheckpointSink::new(
    ///     session.frame_stack_mut(),
    ///     session.graph().name(),
    /// );
    ///
    /// let mut engine = ExecutionEngine::new(
    ///     session.state_mut(),
    ///     Some(stream),       // Stream 由调用者提供
    ///     cancel,
    ///     Some(&mut sink),    // 启用自动 checkpoint
    ///     None,               // 不需要 Barrier Sink
    /// );
    /// session.run_with(&mut engine).await?;
    /// ```
    pub async fn run_with(
        &mut self,
        engine: &mut crate::ExecutionEngine<'_, S>,
    ) -> Result<(), crate::GraphError> {
        self.graph.run_inline(engine, usize::MAX).await
    }
}

// ─── Default for ExecutionSession ──────────────────────────────

impl<S: WorkflowState, M: MergeStrategy<S>> Default for ExecutionSession<S, M>
where
    S: Default,
    S::Checkpoint: Debug,
{
    fn default() -> Self {
        // 注意：Default 需要一个 Graph，这里用空图占位
        // 实际使用时应该用 new(state, graph)
        panic!("ExecutionSession::default() not supported — use new(state, graph)")
    }
}

// ─── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateExt;
    use crate::{GraphBuilder, NodeKind, TaskNode};

    #[test]
    fn test_session_checkpoint_roundtrip() {
        // 创建一个简单的 Graph
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.end("a");
        let graph = Arc::new(builder.build().unwrap());

        // 创建 Session
        let state = State::new();
        let mut session = ExecutionSession::new(state, graph.clone());

        // 添加一些数据到 state
        session
            .state_mut()
            .insert("key".to_string(), serde_json::json!("value"));

        // 创建 checkpoint
        let checkpoint = session.checkpoint();

        // 验证 checkpoint 包含状态
        assert!(checkpoint.state.contains("key"));

        // 从 checkpoint 恢复
        let restored_session =
            ExecutionSession::restore(checkpoint, graph).expect("restore should succeed");

        // 验证恢复后的状态
        assert!(restored_session.state().contains("key"));
    }

    #[test]
    fn test_session_restore_graph_mismatch() {
        // 验证 graph_hash 不匹配时返回错误
        let mut builder1 = GraphBuilder::<State, StateMerge>::new("test1");
        builder1.start("a");
        builder1.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder1.end("a");
        builder1.canonical_hash(0x1111); // 设置不同的 hash
        let graph1 = Arc::new(builder1.build().unwrap());

        let mut builder2 = GraphBuilder::<State, StateMerge>::new("test2");
        builder2.start("b");
        builder2.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        builder2.end("b");
        builder2.canonical_hash(0x2222); // 设置不同的 hash
        let graph2 = Arc::new(builder2.build().unwrap());

        // 用 graph1 创建 checkpoint
        let session = ExecutionSession::new(State::new(), graph1);
        let checkpoint = session.checkpoint();

        // 用 graph2 恢复 — 应该失败
        let result = ExecutionSession::restore(checkpoint, graph2);
        assert!(result.is_err());
        // 验证错误信息包含 "graph hash mismatch"
        match result {
            Err(e) => assert!(format!("{}", e).contains("graph hash mismatch")),
            Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn test_session_into_parts() {
        // 创建一个简单的 Graph
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.end("a");
        let graph = Arc::new(builder.build().unwrap());

        // 创建 Session
        let state = State::new();
        let session = ExecutionSession::new(state, graph);

        // 消费 session，获取 state 和 frame_stack
        let (_state, frame_stack) = session.into_parts();

        // 验证 frame_stack 为空
        assert!(frame_stack.is_empty());
    }

    #[test]
    fn test_session_graph_sharing() {
        // 验证多个 Session 共享同一个 Graph
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.end("a");
        let graph = Arc::new(builder.build().unwrap());

        let session1 = ExecutionSession::new(State::new(), graph.clone());
        let session2 = ExecutionSession::new(State::new(), graph.clone());

        // 验证 Arc 强引用计数
        assert_eq!(Arc::strong_count(&graph), 3); // original + session1 + session2
    }
}
