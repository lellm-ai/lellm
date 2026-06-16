//! 节点核心类型与模块。
//!
//! - `GraphNode` trait, `NextStep` 枚举
//! - `NodeKind` 节点类型枚举
//! - `TaskNode`, `ConditionNode`, `BarrierNode`
//! - 重新导出 `llm_node`, `tool_node`, `barrier_node` 模块

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{GraphError, ObservedError, TerminalError};
use crate::event::{BarrierId, GraphEvent, SpanId};
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

/// 节点流式执行结果。
#[derive(Debug)]
pub enum StreamNodeResult {
    /// 节点正常完成
    Done {
        /// 下一步
        next: NextStep,
        /// 执行实例 ID（由调用方传入）
        span_id: SpanId,
    },
    /// Barrier 暂停，等待外部决策
    BarrierPaused {
        /// Barrier 审批请求 ID（由 executor 生成）
        barrier_id: BarrierId,
        /// 节点名称
        node_name: String,
        /// 执行实例 ID
        span_id: SpanId,
        /// 超时时间（None = 无限等待）
        timeout: Option<std::time::Duration>,
        /// 超时默认行为
        default_action: crate::barrier_node::BarrierDefaultAction,
    },
    /// 观测错误 — 仅事件，不影响 control flow。
    ///
    /// 节点通过此变体声明式地报告非致命异常，executor 负责：
    /// 1. 发送 `GraphEvent::ObservedError` 事件
    /// 2. 按 `next` 继续推进控制流
    Observed {
        /// 观测错误
        error: ObservedError,
        /// 下一步
        next: NextStep,
        /// 执行实例 ID
        span_id: SpanId,
    },
}

/// 节点执行 trait。
#[async_trait]
pub trait GraphNode: Send + Sync {
    /// 执行节点逻辑（阻塞模式）。
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError>;

    /// 执行节点逻辑（流式模式），将内部事件转发到 channel。
    ///
    /// - `sink` — 事件输出 channel
    /// - `span_id` — 执行实例 ID（由 executor 生成）
    ///
    /// 默认实现直接调用 `execute`，返回 `StreamNodeResult::Done`。
    /// AgentNode 覆写此方法以转发 AgentEvent。
    /// BarrierNode 覆写此方法以返回 `StreamNodeResult::BarrierPaused`。
    async fn execute_stream(
        &self,
        state: &mut State,
        _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        span_id: SpanId,
    ) -> Result<StreamNodeResult, GraphError> {
        let next = self.execute(state).await?;
        Ok(StreamNodeResult::Done { next, span_id })
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
    /// Human-in-the-loop 审批屏障（仅流式模式）
    Barrier(BarrierNode),
}

// ─── TaskNode ────────────────────────────────────────────────

/// Task 节点回调类型别名。
/// Arc 包装以支持 Clone。
pub type TaskFn = Arc<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>;

/// 条件分支回调类型别名。
/// Arc 包装以支持 Clone。
pub type BranchCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;

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
            func: Arc::new(func),
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
    /// 兜底目标 — 当所有 branch 条件均不匹配时，跳转到此节点。
    /// 未设置时，无匹配则返回 TerminalError。
    pub otherwise_target: Option<String>,
}

impl ConditionNode {
    pub fn builder(name: impl Into<String>) -> ConditionNodeBuilder {
        ConditionNodeBuilder {
            name: name.into(),
            branches: Vec::new(),
            otherwise_target: None,
        }
    }
}

/// ConditionNode 构建器。
pub struct ConditionNodeBuilder {
    name: String,
    branches: Vec<(String, BranchCondition)>,
    otherwise_target: Option<String>,
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

    /// 设置兜底目标 — 当所有 branch 条件均不匹配时，跳转到此节点。
    ///
    /// 解决"边有 fallback，节点没有"的概念不一致问题。
    ///
    /// ```rust,ignore
    /// ConditionNode::builder("route")
    ///     .branch("fast_path", |s| s.get("score").map(|v| v.as_u64().unwrap_or(0) >= 80))
    ///     .branch("slow_path", |s| s.get("score").map(|v| v.as_u64().unwrap_or(0) >= 50))
    ///     .otherwise("default")  // 兜底
    ///     .build()
    /// ```
    pub fn otherwise(mut self, target: impl Into<String>) -> Self {
        self.otherwise_target = Some(target.into());
        self
    }

    pub fn build(self) -> ConditionNode {
        ConditionNode {
            name: self.name,
            branches: self.branches,
            otherwise_target: self.otherwise_target,
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
        // 有兜底目标 → 直接跳转
        if let Some(ref target) = self.otherwise_target {
            return Ok(NextStep::Goto(target.clone()));
        }
        Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
            node: self.name.clone(),
            source: "no matching branch and no otherwise target".into(),
        }))
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
            Self::Barrier(n) => n.execute(state).await,
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
            Self::Agent(n) => n.execute_stream(state, sink, span_id).await,
            Self::Tool(n) => n.execute_stream(state, sink, span_id).await,
            Self::Condition(n) => n.execute_stream(state, sink, span_id).await,
            Self::Barrier(n) => n.execute_stream(state, sink, span_id).await,
        }
    }
}
