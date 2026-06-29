//! 节点核心类型与模块。
//!
//! v0.4 终态架构：
//!
//! - `LeafNode<S>` — 声明式业务节点，只能读 State + emit Mutation
//! - `ExecutorOperation<S>` — 命令式执行控制，可以 clone/merge/replace_state
//! - `NodeKind<S, M>` — Graph 的 AST（不实现任何执行 trait）
//! - `FlowNode<S>` — 向后兼容，等同于 LeafNode
//!
//! 职责边界：
//!
//! ```text
//! Graph (AST)
//!     └── NodeKind
//!
//! ExecutionEngine (runtime owner)
//!     ├── dispatch → match NodeKind
//!     ├── build_leaf_context() → LeafNode
//!     └── pass &mut self → ExecutorOperation
//!
//! LeafNode
//!     └── 只能 emit Mutation
//!
//! ExecutorOperation
//!     └── 可以操纵 Executor（fork / merge / replace_state / subgraph）
//! ```

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::GraphError;
pub use crate::node_context::LeafContext;
use crate::node_context::{ExecutionEngine, NodeContext};
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};

// ─── 子模块重新导出 ────────────────────────────────────────────

pub use crate::barrier_node::{BarrierDefaultAction, BarrierNode};
pub use crate::parallel_node::{
    ParallelErrorStrategy, ParallelNode, ParallelNodeBuilder,
};

// ─── LeafNode Trait ───────────────────────────────────────────

/// 声明式业务节点 — 只能读 State + emit Mutation。
///
/// 设计原则：
/// - **只能读 State**（`ctx.state()` 返回 `&S`）
/// - **只能 emit Mutation**（`ctx.record()`）
/// - **不能 replace_state / clone_state / fork / merge**
///
/// 与 `ExecutorOperation` 完全不同维度：
/// - LeafNode = 业务逻辑（Task, Condition, Barrier, LLM, Tool）
/// - ExecutorOperation = 执行控制（Parallel, Retry, Loop, SubGraph）
///
/// # 泛型参数
///
/// - `S` — 类型化状态（默认 `State` = HashMap，向后兼容）
#[async_trait]
pub trait LeafNode<S: WorkflowState = State>: Send + Sync {
    /// 执行节点逻辑。
    async fn execute(&self, ctx: &mut LeafContext<'_, S>) -> Result<(), GraphError>;
}

// ─── ExecutorOperation Trait ──────────────────────────────────

/// 命令式执行控制 — Composite 节点使用。
///
/// 直接接收 `&mut ExecutionEngine<S>`，拥有完整能力：
/// - clone_state / replace_state
/// - spawn_child_engine
/// - merge_state
/// - build_leaf_context（用于执行子分支）
///
/// 这不是"节点"，而是 ExecutionEngine 的内部控制逻辑扩展。
#[async_trait]
pub trait ExecutorOperation<S: WorkflowState = State>: Send + Sync {
    /// 执行组合操作。
    async fn execute(&self, engine: &mut ExecutionEngine<S>) -> Result<(), GraphError>;
}

// ─── Backward Compat: FlowNode ────────────────────────────────

/// 向后兼容 — `FlowNode` trait。
///
/// 保留此名称以兼容现有代码。
/// 接收 `NodeContext`（持有 `&mut S`），以便旧代码继续工作。
#[async_trait]
pub trait FlowNode<S: WorkflowState = State>: Send + Sync {
    /// 执行节点逻辑。
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError>;
}

// ─── NodeKind (AST Only) ──────────────────────────────────────

/// Graph 的 AST — 节点类型枚举。
///
/// **不实现任何执行 trait。** 它只是数据结构。
/// 执行分发由 ExecutionEngine 的 match 负责。
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
    /// 外部节点（由 lellm-agent 等 crate 提供）— 向后兼容，使用 NodeContext
    External(Arc<dyn FlowNode<S>>),
    /// 外部 Leaf 节点 — 只能读 State + emit Mutation
    ExternalLeaf(Arc<dyn LeafNode<S>>),
}

impl<S: WorkflowState, M: MergeStrategy<S>> Clone for NodeKind<S, M> {
    fn clone(&self) -> Self {
        match self {
            Self::Task(n) => Self::Task(n.clone()),
            Self::Condition(n) => Self::Condition(n.clone()),
            Self::Barrier(n) => Self::Barrier(n.clone()),
            Self::Parallel(n) => Self::Parallel(n.clone()),
            Self::External(n) => Self::External(n.clone()),
            Self::ExternalLeaf(n) => Self::ExternalLeaf(n.clone()),
        }
    }
}

// ─── TaskNode ────────────────────────────────────────────────

/// Task 节点回调类型别名（向后兼容 NodeContext）。
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

/// TaskNode 实现 FlowNode（向后兼容 — 使用 NodeContext）。
///
/// 未来将迁移到 LeafNode + LeafContext。
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

/// ConditionNode 实现 FlowNode（向后兼容 — 使用 NodeContext）。
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

// ─── Backward Compatibility Alias ─────────────────────────────

/// 向后兼容别名 — `GraphNode` → `FlowNode`。
pub type GraphNode<S> = dyn FlowNode<S>;
