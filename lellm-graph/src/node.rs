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

use crate::error::{GraphError, ObservedError};
use crate::event::{BarrierId, GraphEvent};
use crate::state::{State, SpanId};

// ─── 子模块重新导出 ────────────────────────────────────────────

pub use crate::barrier_node::{BarrierDefaultAction, BarrierNode};

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

/// 节点流式执行结果。
#[derive(Debug)]
pub enum StreamNodeResult {
    /// 节点正常完成（统一 Done + Observed）
    Continue {
        /// 下一步
        next: NextStep,
        /// 执行实例 ID
        span_id: SpanId,
        /// 可选的观测错误（不影响 control flow）
        observed: Option<ObservedError>,
    },
    /// Barrier 暂停，等待外部决策
    Pause {
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
        /// 降级原因
        reason: String,
        /// 节点名称
        node_name: String,
    },
}

/// 节点执行 trait — trait-based 设计。
///
/// Graph 只知道 `dyn FlowNode`，不知道 `AgentNode`、`ToolNode` 等具体类型。
/// `AgentFlowNode` 由 `lellm-agent` crate 提供。
#[async_trait]
pub trait FlowNode: Send + Sync {
    /// 执行节点逻辑（阻塞模式）。
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError>;

    /// 执行节点逻辑（流式模式），将内部事件转发到 channel。
    ///
    /// - `sink` — 事件输出 channel
    /// - `span_id` — 执行实例 ID（由 executor 生成）
    ///
    /// 默认实现直接调用 `execute`，返回 `StreamNodeResult::Continue`。
    /// BarrierNode 覆写此方法以返回 `StreamNodeResult::Pause`。
    async fn execute_stream(
        &self,
        state: &mut State,
        _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        span_id: SpanId,
    ) -> Result<StreamNodeResult, GraphError> {
        let next = self.execute(state).await?;
        Ok(StreamNodeResult::Continue {
            next,
            span_id,
            observed: None,
        })
    }
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
    /// 外部节点（由 lellm-agent 等 crate 提供）
    ///
    /// 使用 `Arc<dyn FlowNode>` 让 Graph 不知道具体节点类型，同时支持 Clone。
    External(std::sync::Arc<dyn FlowNode>),
}

// ─── TaskNode ────────────────────────────────────────────────

/// Task 节点回调类型别名。
/// Arc 包装以支持 Clone。
pub type TaskFn = Arc<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>;

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
        func: impl Fn(&mut State) -> Result<(), GraphError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            func: Arc::new(func),
        }
    }
}

#[async_trait]
impl FlowNode for TaskNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        (self.func)(state)?;
        Ok(NextStep::GoToNext)
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
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        for (target, condition) in &self.branches {
            if condition(state) {
                return Ok(NextStep::Goto(target.clone()));
            }
        }
        // 无匹配 → GoToNext，由 Graph 层 edge_fallback 处理兜底
        Ok(NextStep::GoToNext)
    }
}

// ─── NodeKind FlowNode impl ──────────────────────────────────

#[async_trait]
impl FlowNode for NodeKind {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        match self {
            Self::Task(n) => n.execute(state).await,
            Self::Condition(n) => n.execute(state).await,
            Self::Barrier(n) => n.execute(state).await,
            Self::External(n) => n.execute(state).await,
        }
    }

    async fn execute_stream(
        &self,
        state: &mut State,
        sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        span_id: SpanId,
    ) -> Result<StreamNodeResult, GraphError> {
        match self {
            Self::Task(n) => n.execute_stream(state, sink, span_id).await,
            Self::Condition(n) => n.execute_stream(state, sink, span_id).await,
            Self::Barrier(n) => n.execute_stream(state, sink, span_id).await,
            Self::External(n) => n.execute_stream(state, sink, span_id).await,
        }
    }
}

// ─── Backward Compatibility Alias ─────────────────────────────

/// 向后兼容别名 — `GraphNode` → `FlowNode`。
///
/// v0.2 代码使用 `GraphNode`，v0.3 统一为 `FlowNode`。
pub type GraphNode = dyn FlowNode;
