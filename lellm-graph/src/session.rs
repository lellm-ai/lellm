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

use serde::{Deserialize, Serialize};

use crate::checkpoint::FrameStack;
use crate::graph::Graph;
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};

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

/// 执行会话 — 持有 State 所有权 + FrameStack + Graph。
///
/// # 职责
///
/// - 持有 State 所有权（Engine 只是借用）
/// - 管理 FrameStack（Subgraph 执行时 push/pop）
/// - 创建和恢复 SessionCheckpoint
///
/// # 与 ExecutionEngine 的关系
///
/// ```text
/// ExecutionSession
/// ├── state: S           （拥有所有权）
/// ├── frame_stack:       （执行位置历史）
/// └── graph: Graph<S>    （图结构）
///
/// └── session.run()
///     ├── engine: ExecutionEngine<'_, S>  （借用 &mut state）
///     └── graph.run_inline(&mut engine)
/// ```
pub struct ExecutionSession<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge>
where
    S::Checkpoint: Debug,
{
    /// 运行时状态（拥有所有权）
    state: S,
    /// 执行帧栈（Subgraph 执行时 push/pop）
    frame_stack: FrameStack<S>,
    /// 图结构
    graph: Graph<S, M>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> ExecutionSession<S, M>
where
    S::Checkpoint: Debug,
{
    /// 创建新的执行会话。
    pub fn new(state: S, graph: Graph<S, M>) -> Self {
        Self {
            state,
            frame_stack: FrameStack::new(),
            graph,
        }
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

    /// 从 checkpoint 恢复。
    ///
    /// # P0-1: 使用 restore() 恢复 State
    ///
    /// `S::restore(checkpoint.state)` 从 checkpoint snapshot 恢复完整 Runtime State。
    pub fn restore(checkpoint: SessionCheckpoint<S>, graph: Graph<S, M>) -> Self {
        // P0-2: 校验 graph_hash
        if checkpoint.graph_hash != graph.canonical_hash() {
            tracing::warn!(
                expected = format!("{:#018x}", checkpoint.graph_hash),
                actual = format!("{:#018x}", graph.canonical_hash()),
                "graph hash mismatch during restore"
            );
        }

        let state = S::restore(checkpoint.state);
        Self {
            state,
            frame_stack: checkpoint.frames,
            graph,
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
    /// 内联执行 — 借用 state 给 Engine 执行。
    ///
    /// 这是便捷方法，内部创建临时 Engine 执行 graph。
    pub async fn run_inline(&mut self, max_steps: usize) -> Result<(), crate::GraphError> {
        let mut engine =
            crate::ExecutionEngine::new(&mut self.state, None, crate::CancellationToken::new());
        self.graph.run_inline(&mut engine, max_steps).await
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
        let graph = builder.build().unwrap();

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
        let restored_session = ExecutionSession::restore(checkpoint, graph);

        // 验证恢复后的状态
        assert!(restored_session.state().contains("key"));
    }

    #[test]
    fn test_session_into_parts() {
        // 创建一个简单的 Graph
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.end("a");
        let graph = builder.build().unwrap();

        // 创建 Session
        let state = State::new();
        let session = ExecutionSession::new(state, graph);

        // 消费 session，获取 state 和 frame_stack
        let (_state, frame_stack) = session.into_parts();

        // 验证 frame_stack 为空
        assert!(frame_stack.is_empty());
    }
}
