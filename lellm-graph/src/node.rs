//! 节点核心类型与模块。
//!
//! - `FlowNode<S>` trait — trait-based 节点，Graph 不知道具体节点类型
//! - `NodeKind<S>` 节点类型枚举（Task, Condition, Barrier, Parallel, External）
//! - `TaskNode<S>`, `ConditionNode<S>`
//!
//! v0.4+: 所有节点类型泛型化 `S: WorkflowState`，默认 `S = State`（向后兼容）。

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::GraphError;
use crate::node_context::NodeContext;
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};

// ─── 子模块重新导出 ────────────────────────────────────────────

pub use crate::barrier_node::{BarrierDefaultAction, BarrierNode};
pub use crate::parallel_node::{
    ParallelErrorStrategy, ParallelNode, ParallelNodeBuilder,
};

// ─── v04 FlowNode Trait ──────────────────────────────────────

/// v04 节点执行 trait — Context 驱动一切。
///
/// 统一原则 — 节点不返回业务数据，只返回 `Result<(), GraphError>`：
/// - State      → ctx.state()（只读）
/// - Mutation   → ctx.record()（唯一写入口）
/// - Stream     → ctx.emit()
/// - Metadata   → ctx.set_token_cost()
/// - Control    → ctx.goto() / ctx.end() / ctx.pause()
///
/// # 泛型参数
///
/// - `S` — 类型化状态（默认 `State` = HashMap，向后兼容）
#[async_trait]
pub trait FlowNode<S: WorkflowState = State>: Send + Sync {
    /// 执行节点逻辑。
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError>;
}

// ─── NodeKind ─────────────────────────────────────────────────

/// 节点类型枚举。
///
/// # 泛型参数
///
/// - `S` — 类型化状态（默认 `State` = HashMap，向后兼容）
/// - `M` — 并行合并策略（仅 `Parallel` 变体使用，默认 [`StateMerge`]）
pub enum NodeKind<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    /// 自定义逻辑
    Task(TaskNode<S>),
    /// 条件分支
    Condition(ConditionNode<S>),
    /// Human-in-the-loop 审批屏障
    Barrier(BarrierNode<S>),
    /// 并行执行多个分支
    Parallel(ParallelNode<S, M>),
    /// 外部节点（由 lellm-agent 等 crate 提供）
    External(Arc<dyn FlowNode<S>>),
}

impl<S: WorkflowState, M: MergeStrategy<S>> Clone for NodeKind<S, M> {
    fn clone(&self) -> Self {
        match self {
            Self::Task(n) => Self::Task(n.clone()),
            Self::Condition(n) => Self::Condition(n.clone()),
            Self::Barrier(n) => Self::Barrier(n.clone()),
            Self::Parallel(n) => Self::Parallel(n.clone()),
            Self::External(n) => Self::External(n.clone()),
        }
    }
}

// ─── TaskNode ────────────────────────────────────────────────

/// Task 节点回调类型别名。
pub type TaskFn<S> = Arc<dyn Fn(&mut NodeContext<'_, S>) -> Result<(), GraphError> + Send + Sync>;

/// 自定义逻辑节点。
#[derive(Clone)]
pub struct TaskNode<S: WorkflowState = State> {
    pub name: String,
    pub func: TaskFn<S>,
}

impl<S: WorkflowState> TaskNode<S> {
    pub fn new(
        name: impl Into<String>,
        func: impl Fn(&mut NodeContext<'_, S>) -> Result<(), GraphError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            func: Arc::new(func),
        }
    }
}

#[async_trait]
impl<S: WorkflowState> FlowNode<S> for TaskNode<S> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        (self.func)(ctx)
    }
}

// ─── ConditionNode ───────────────────────────────────────────

/// 条件分支回调类型别名。
pub type BranchCondition<S> = Arc<dyn Fn(&S) -> bool + Send + Sync>;

/// 条件分支节点。
#[derive(Clone)]
pub struct ConditionNode<S: WorkflowState = State> {
    pub name: String,
    pub branches: Vec<(String, BranchCondition<S>)>,
}

impl<S: WorkflowState> ConditionNode<S> {
    pub fn builder(name: impl Into<String>) -> ConditionNodeBuilder<S> {
        ConditionNodeBuilder {
            name: name.into(),
            branches: Vec::new(),
        }
    }
}

/// ConditionNode 构建器。
pub struct ConditionNodeBuilder<S: WorkflowState = State> {
    name: String,
    branches: Vec<(String, BranchCondition<S>)>,
}

impl<S: WorkflowState> ConditionNodeBuilder<S> {
    pub fn branch(
        mut self,
        target: impl Into<String>,
        condition: impl Fn(&S) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.branches.push((target.into(), Arc::new(condition)));
        self
    }

    pub fn build(self) -> ConditionNode<S> {
        ConditionNode {
            name: self.name,
            branches: self.branches,
        }
    }
}

#[async_trait]
impl<S: WorkflowState> FlowNode<S> for ConditionNode<S> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        let state = ctx.state();
        for (target, condition) in &self.branches {
            if condition(state) {
                ctx.goto(target);
                return Ok(());
            }
        }
        Ok(())
    }
}

// ─── NodeKind FlowNode impl ──────────────────────────────────

#[async_trait]
impl<S: WorkflowState, M: MergeStrategy<S>> FlowNode<S> for NodeKind<S, M> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        match self {
            Self::Task(n) => n.execute(ctx).await,
            Self::Condition(n) => n.execute(ctx).await,
            Self::Barrier(n) => n.execute(ctx).await,
            Self::Parallel(n) => n.execute(ctx).await,
            Self::External(n) => n.execute(ctx).await,
        }
    }
}

// ─── Backward Compatibility Alias ─────────────────────────────

/// 向后兼容别名 — `GraphNode` → `FlowNode`。
pub type GraphNode<S> = dyn FlowNode<S>;
