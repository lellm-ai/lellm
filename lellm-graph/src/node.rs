//! 节点核心类型与模块。
//!
//! - `GraphNode` trait, `NextStep` 枚举
//! - `NodeKind` 节点类型枚举
//! - `TaskNode`, `ConditionNode`, `LoopNode`, `SubGraph`, `BarrierNode`
//! - 重新导出 `llm_node`, `tool_node`, `barrier_node` 模块

use async_trait::async_trait;

use crate::error::GraphError;
use crate::event::GraphEvent;
use crate::graph::Edge;
use crate::state::State;

// ─── 子模块重新导出 ────────────────────────────────────────────

pub use crate::barrier_node::{BarrierDefaultAction, BarrierNode};
pub use crate::llm_node::{AgentNode, LLMNode};
pub use crate::tool_node::ToolNode;

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

/// 节点执行 trait。
#[async_trait]
pub trait GraphNode: Send + Sync {
    /// 执行节点逻辑（阻塞模式）。
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError>;

    /// 执行节点逻辑（流式模式），将内部事件转发到 channel。
    ///
    /// 默认实现直接调用 `execute`，不产生流式事件。
    /// AgentNode 覆写此方法以转发 AgentEvent。
    async fn execute_stream(
        &self,
        state: &mut State,
        _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
    ) -> Result<NextStep, GraphError> {
        self.execute(state).await
    }
}

/// 节点类型枚举。
pub enum NodeKind {
    /// 自定义逻辑
    Task(TaskNode),
    /// Agent（包装 ToolUseLoop）
    Agent(Box<AgentNode>),
    /// 工具调用
    Tool(ToolNode),
    /// 条件分支
    Condition(ConditionNode),
    /// 循环容器
    Loop(Box<LoopNode>),
    /// Human-in-the-loop 审批屏障（仅流式模式）
    Barrier(BarrierNode),
}

// ─── TaskNode ────────────────────────────────────────────────

/// Task 节点回调类型别名。
pub type TaskFn = Box<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>;

/// 条件分支回调类型别名。
pub type BranchCondition = Box<dyn Fn(&State) -> bool + Send + Sync>;

/// 自定义逻辑节点。
pub struct TaskNode {
    pub name: String,
    pub func: TaskFn,
}

impl TaskNode {
    pub fn new(
        name: impl Into<String>,
        func: impl Fn(&mut State) -> Result<(), GraphError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            func: Box::new(func),
        }
    }
}

#[async_trait]
impl GraphNode for TaskNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        (self.func)(state)?;
        Ok(NextStep::GoToNext)
    }
}

// ─── ConditionNode ───────────────────────────────────────────

/// 条件分支节点。
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
        self.branches.push((target.into(), Box::new(condition)));
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
impl GraphNode for ConditionNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        for (target, condition) in &self.branches {
            if condition(state) {
                return Ok(NextStep::Goto(target.clone()));
            }
        }
        Err(GraphError::NodeExecutionFailed {
            node: self.name.clone(),
            source: "no matching branch".into(),
        })
    }
}

// ─── SubGraph ────────────────────────────────────────────────

/// 子图（LoopNode 的执行单元）。
///
/// **注意：** SubGraph 内的节点不支持按名跳转（`NextStep::Goto`），
/// 因为节点没有名字。需要条件回跳请使用外层 Graph 的 `edge_if`。
#[derive(Default)]
pub struct SubGraph {
    pub nodes: Vec<Box<dyn GraphNode>>,
    pub edges: Vec<Edge>,
}

impl SubGraph {
    pub fn new() -> Self {
        Self::default()
    }

    /// 线性执行子图内所有节点，尊重 `NextStep` 语义。
    ///
    /// - `GoToNext` — 继续遍历下一个节点
    /// - `End` — 提前退出子图（后续节点不再执行）
    /// - `Goto(target)` — 报错（SubGraph 不支持按名跳转）
    pub async fn execute(&self, state: &mut State) -> Result<(), GraphError> {
        for node in &self.nodes {
            match node.execute(state).await? {
                NextStep::GoToNext => {
                    // 继续线性遍历
                }
                NextStep::End => {
                    // 提前退出子图
                    break;
                }
                NextStep::Goto(target) => {
                    return Err(GraphError::InvalidGraph(format!(
                        "SubGraph does not support Goto(\"{}\"). Use Graph::edge_if for conditional jumps.",
                        target
                    )));
                }
            }
        }
        Ok(())
    }
}

// ─── LoopNode ────────────────────────────────────────────────

/// 循环容器 — 可选的高级语法糖。
///
/// **推荐使用 `edge_if` 实现简单回跳。** LoopNode 适用于需要独立迭代计数
/// 和独立熔断保护的封装场景（例如并行子任务中的局部循环）。
///
/// ```rust,ignore
/// // 推荐：直接用有环图 + edge_if（更直观）
/// GraphBuilder::new("retry")
///     .edge_if("check", "agent", |s| !s.satisfied)  // 回跳
///     .edge("check", "output")                       // 通过
///
/// // LoopNode：需要独立 max_iterations 时使用
/// LoopNode::new("loop", SubGraph { ... }, |s| !s.satisfied, max_iterations: 5)
/// ```
pub struct LoopNode {
    pub name: String,
    pub body: SubGraph,
    pub continue_condition: Box<dyn Fn(&State) -> bool + Send + Sync>,
    pub max_iterations: usize,
}

impl LoopNode {
    pub fn new(
        name: impl Into<String>,
        body: SubGraph,
        continue_condition: impl Fn(&State) -> bool + Send + Sync + 'static,
        max_iterations: usize,
    ) -> Self {
        Self {
            name: name.into(),
            body,
            continue_condition: Box::new(continue_condition),
            max_iterations,
        }
    }
}

#[async_trait]
impl GraphNode for LoopNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        for i in 0..self.max_iterations {
            tracing::debug!(
                loop_name = %self.name,
                iteration = i + 1,
                max = self.max_iterations,
                "executing loop body"
            );

            self.body.execute(state).await?;

            if !(self.continue_condition)(state) {
                tracing::debug!(
                    loop_name = %self.name,
                    iterations = i + 1,
                    "loop condition met, exiting"
                );
                return Ok(NextStep::GoToNext);
            }
        }

        Err(GraphError::LoopLimitExceeded {
            limit: self.max_iterations,
        })
    }
}

// ─── NodeKind GraphNode impl ─────────────────────────────────

#[async_trait]
impl GraphNode for NodeKind {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        match self {
            Self::Task(n) => n.execute(state).await,
            Self::Agent(n) => n.execute(state).await,
            Self::Tool(n) => n.execute(state).await,
            Self::Condition(n) => n.execute(state).await,
            Self::Loop(n) => n.execute(state).await,
            Self::Barrier(n) => n.execute(state).await,
        }
    }

    async fn execute_stream(
        &self,
        state: &mut State,
        sink: &tokio::sync::mpsc::Sender<GraphEvent>,
    ) -> Result<NextStep, GraphError> {
        match self {
            Self::Task(n) => n.execute_stream(state, sink).await,
            Self::Agent(n) => n.execute_stream(state, sink).await,
            Self::Tool(n) => n.execute_stream(state, sink).await,
            Self::Condition(n) => n.execute_stream(state, sink).await,
            Self::Loop(n) => n.execute_stream(state, sink).await,
            Self::Barrier(n) => n.execute_stream(state, sink).await,
        }
    }
}
