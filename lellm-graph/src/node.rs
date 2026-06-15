//! 节点类型定义。

use async_trait::async_trait;

use crate::error::GraphError;
use crate::graph::Edge;
use crate::state::State;

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
    /// 执行节点逻辑。
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError>;
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

// ─── AgentNode ───────────────────────────────────────────────

/// Agent 节点（包装 ToolUseLoop）。
pub struct AgentNode {
    pub name: String,
    pub agent: lellm_agent::ToolUseLoop,
    pub messages_key: String,
    pub output_key: String,
}

impl AgentNode {
    pub fn new(name: impl Into<String>, agent: lellm_agent::ToolUseLoop) -> Self {
        Self {
            name: name.into(),
            agent,
            messages_key: "messages".into(),
            output_key: "output".into(),
        }
    }

    pub fn with_messages_key(mut self, key: impl Into<String>) -> Self {
        self.messages_key = key.into();
        self
    }

    pub fn with_output_key(mut self, key: impl Into<String>) -> Self {
        self.output_key = key.into();
        self
    }
}

#[async_trait]
impl GraphNode for AgentNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        let messages = state
            .get(&self.messages_key)
            .and_then(|v| serde_json::from_value::<Vec<lellm_core::Message>>(v.clone()).ok())
            .unwrap_or_default();

        let result =
            self.agent
                .execute(messages)
                .await
                .map_err(|e| GraphError::NodeExecutionFailed {
                    node: self.name.clone(),
                    source: Box::new(e),
                })?;

        let text: String = result
            .response
            .content
            .iter()
            .filter_map(|b| match b {
                lellm_core::ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        if !text.is_empty() {
            state.insert(self.output_key.clone(), serde_json::Value::String(text));
        }

        Ok(NextStep::GoToNext)
    }
}

// ─── ToolNode ────────────────────────────────────────────────

/// 工具调用节点。
pub struct ToolNode {
    pub name: String,
    pub tool_name: String,
    pub args_key: String,
    pub output_key: String,
}

impl ToolNode {
    pub fn new(name: impl Into<String>, tool_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tool_name: tool_name.into(),
            args_key: "tool_args".into(),
            output_key: "tool_output".into(),
        }
    }
}

#[async_trait]
impl GraphNode for ToolNode {
    async fn execute(&self, _state: &mut State) -> Result<NextStep, GraphError> {
        tracing::warn!(tool = %self.tool_name, "ToolNode::execute not yet implemented");
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
#[derive(Default)]
pub struct SubGraph {
    pub nodes: Vec<Box<dyn GraphNode>>,
    pub edges: Vec<Edge>,
}

impl SubGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn execute(&self, state: &mut State) -> Result<(), GraphError> {
        for node in &self.nodes {
            node.execute(state).await?;
        }
        Ok(())
    }
}

// ─── LoopNode ────────────────────────────────────────────────

/// 循环容器。
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
        }
    }
}
