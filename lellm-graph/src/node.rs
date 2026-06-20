//! 节点核心类型与模块。
//!
//! - `FlowNode` trait — trait-based 节点，Graph 不知道具体节点类型
//! - `NextStep` 枚举，`StreamNodeResult` 枚举
//! - `NodeKind` 节点类型枚举（Task, Condition, Barrier）
//! - `TaskNode`, `ConditionNode`
//!
//! AgentNode → AgentFlowNode（由 lellm-agent 提供，实现 FlowNode trait）

use std::sync::Arc;

use async_trait::async_trait;

use crate::delta::StateDelta;
use crate::error::GraphError;
use crate::event::BarrierId;
use crate::node_context::NodeContext;
use crate::state::State;

// ─── 子模块重新导出 ────────────────────────────────────────────

pub use crate::barrier_node::{BarrierDefaultAction, BarrierNode};
pub use crate::parallel_node::{ParallelErrorStrategy, ParallelNode, ParallelNodeBuilder};

// ─── 核心类型 ──────────────────────────────────────────────────

/// 节点执行后的下一步。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextStep {
    /// 跳转到指定节点
    Goto(String),
    /// 跳转到下一个节点（按拓扑顺序）
    GoToNext,
    /// 结束执行
    End,
}

/// 节点执行输出 — 修改意图 + 下一步。
///
/// 节点不再直接修改 State（`&mut State`），而是输出 `Vec<StateDelta>`。
/// Executor 收集所有 Delta 后统一 apply 到 State。
#[derive(Debug)]
pub struct NodeOutput {
    /// 状态增量（节点对 State 的修改意图）
    pub deltas: Vec<StateDelta>,
    /// 下一步路由
    pub next: NextStep,
    /// 节点元数据（可选 — 用于 Adaptive Checkpoint 等）
    pub metadata: Option<NodeMetadata>,
}

/// 节点执行元数据 — 提供给 Executor 的额外信息。
#[derive(Debug, Clone, Default)]
pub struct NodeMetadata {
    /// Token 消耗成本（0.0 表示无 LLM 调用）
    pub token_cost: f64,
    /// 是否有外部副作用（如部署、发送消息）
    pub has_side_effects: bool,
}

impl NodeOutput {
    /// 创建无 Delta 的输出。
    pub fn new(next: NextStep) -> Self {
        Self {
            deltas: Vec::new(),
            next,
            metadata: None,
        }
    }

    /// 追加一个 Delta。
    pub fn with_delta(mut self, delta: StateDelta) -> Self {
        self.deltas.push(delta);
        self
    }

    /// 追加多个 Delta。
    pub fn with_deltas(mut self, deltas: Vec<StateDelta>) -> Self {
        self.deltas.extend(deltas);
        self
    }

    /// 设置节点元数据。
    pub fn with_metadata(mut self, metadata: NodeMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// 设置 token 成本。
    pub fn with_token_cost(mut self, cost: f64) -> Self {
        self.metadata
            .get_or_insert_with(Default::default)
            .token_cost = cost;
        self
    }

    /// 标记有副作用。
    pub fn with_side_effects(mut self) -> Self {
        self.metadata
            .get_or_insert_with(Default::default)
            .has_side_effects = true;
        self
    }
}

/// 节点流式执行结果。
#[derive(Debug)]
pub enum StreamNodeResult {
    /// 节点正常完成（统一 Done + Observed）
    Continue {
        /// 状态增量
        deltas: Vec<StateDelta>,
        /// 下一步
        next: NextStep,
        /// 执行实例 ID
        span_id: SpanId,
        /// 可选的观测错误（不影响 control flow）
        observed: Option<ObservedError>,
        /// 节点元数据（可选 — 用于 Adaptive Checkpoint 等）
        metadata: Option<NodeMetadata>,
    },
    /// Barrier 暂停，等待外部决策
    Pause {
        /// 状态增量（Barrier 进入等待前的修改）
        deltas: Vec<StateDelta>,
        /// Barrier 审批请求 ID
        barrier_id: BarrierId,
        /// 节点名称
        node_name: String,
        /// 执行实例 ID
        span_id: SpanId,
        /// 超时时间（None = 无限等待）
        timeout: Option<std::time::Duration>,
        /// 超时默认行为
        default_action: BarrierDefaultAction,
    },
    /// 节点主动声明走备用路径（控制流，非错误）。
    ///
    /// 与 `GraphError::Terminal` 不同：Fallback 是节点主动声明的降级策略，
    /// executor 根据 fallback 边路由到备用节点。
    Fallback {
        /// 状态增量（Fallback 前的修改）
        deltas: Vec<StateDelta>,
        /// 降级原因
        reason: String,
        /// 节点名称
        node_name: String,
    },
}

// ─── v04 FlowNode Trait ──────────────────────────────────────

/// v04 节点执行 trait — Context 驱动一切。
///
/// 统一原则 — 节点不返回业务数据，只返回 `Result<(), GraphError>`：
/// - State      → ctx.set() / ctx.append() / ctx.increment()
/// - Stream     → ctx.emit()
/// - Metadata   → ctx.set_token_cost()
/// - Control    → ctx.goto() / ctx.end() / ctx.pause()
#[async_trait]
pub trait FlowNode: Send + Sync {
    /// 执行节点逻辑。
    ///
    /// - `ctx` — 节点上下文，包含 State、StreamEmitter、控制信号
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError>;
}

/// 节点类型枚举。
///
/// 只包含 Graph 内置节点类型。Agent/LLM/Tool 节点由外部 crate 提供。
///
/// 注意：External 使用 Arc 以支持 Clone（Graph 需要 Clone 来构建）。
#[derive(Clone)]
pub enum NodeKind {
    /// 自定义逻辑
    Task(TaskNode),
    /// 条件分支
    Condition(ConditionNode),
    /// Human-in-the-loop 审批屏障（仅流式模式）
    Barrier(BarrierNode),
    /// 并行执行多个分支，合并 StateDelta
    Parallel(ParallelNode),
    /// 外部节点（由 lellm-agent 等 crate 提供）
    ///
    /// 使用 `Arc<dyn FlowNode>` 让 Graph 不知道具体节点类型，同时支持 Clone。
    External(std::sync::Arc<dyn FlowNode>),
}

// ─── TaskNode ────────────────────────────────────────────────

/// Task 节点回调类型别名。
///
/// 闭包接收 NodeContext，返回 `()` 作为修改意图。
/// Arc 包装以支持 Clone。
pub type TaskFn = Arc<dyn Fn(&mut NodeContext<'_>) -> Result<(), GraphError> + Send + Sync>;

/// 条件分支回调类型别名。
/// Arc 包装以支持 Clone。
pub type BranchCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;

/// 自定义逻辑节点。
#[derive(Clone)]
pub struct TaskNode {
    pub name: String,
    pub func: TaskFn,
}

impl TaskNode {
    pub fn new(
        name: impl Into<String>,
        func: impl Fn(&mut NodeContext<'_>) -> Result<(), GraphError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            func: Arc::new(func),
        }
    }
}

#[async_trait]
impl FlowNode for TaskNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        (self.func)(ctx)
    }
}

// ─── ConditionNode ───────────────────────────────────────────

/// 条件分支节点。
///
/// 按声明顺序求值分支条件，返回第一个匹配分支的 `NextStep::Goto(target)`。
/// 无匹配时返回 `NextStep::GoToNext`，由 Graph 层的 `edge_fallback` 处理兜底路由。
#[derive(Clone)]
pub struct ConditionNode {
    pub name: String,
    pub branches: Vec<(String, BranchCondition)>,
}

impl ConditionNode {
    pub fn builder(name: impl Into<String>) -> ConditionNodeBuilder {
        ConditionNodeBuilder {
            name: name.into(),
            branches: Vec::new(),
        }
    }
}

/// ConditionNode 构建器。
pub struct ConditionNodeBuilder {
    name: String,
    branches: Vec<(String, BranchCondition)>,
}

impl ConditionNodeBuilder {
    pub fn branch(
        mut self,
        target: impl Into<String>,
        condition: impl Fn(&State) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.branches.push((target.into(), Arc::new(condition)));
        self
    }

    pub fn build(self) -> ConditionNode {
        ConditionNode {
            name: self.name,
            branches: self.branches,
        }
    }
}

#[async_trait]
impl FlowNode for ConditionNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        for (target, condition) in &self.branches {
            if condition(ctx.state()) {
                ctx.goto(target);
                return Ok(());
            }
        }
        // 无匹配 → GoToNext，由 Graph 层 edge_fallback 处理兜底
        Ok(())
    }
}

// ─── NodeKind FlowNode impl ──────────────────────────────────

#[async_trait]
impl FlowNode for NodeKind {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
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
///
/// v0.2 代码使用 `GraphNode`，v0.3 统一为 `FlowNode`。
pub type GraphNode = dyn FlowNode;
