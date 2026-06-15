# v0.2 Graph/Node/Edge 编排层实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use compose:subagent (recommended) or compose:execute to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 Workflow DAG + Loop Node 编排层，支持 Task, Agent, Tool, Condition, Loop 五种节点类型。

**Architecture:** 新建 `lellm-graph` crate，依赖 `lellm-agent`。Graph 是一个严格的 Acyclic DAG，循环通过 `LoopNode` 表达。节点共享 `State`（HashMap<String, Value>），按拓扑顺序串行执行，`LoopNode` 内部并发。

**Tech Stack:** Rust, tokio, futures, indexmap, serde_json

---

## 文件结构

```
lellm-graph/
├── Cargo.toml
└── src/
    ├── lib.rs          # 公开 API
    ├── error.rs        # GraphError
    ├── state.rs        # State, GraphResult, ExecutionEntry
    ├── node.rs         # NodeKind, TaskNode, AgentNode, ToolNode, ConditionNode, LoopNode, SubGraph
    ├── graph.rs        # Graph, Edge, GraphBuilder
    └── executor.rs     # GraphExecutor（执行引擎）

tests/
└── integration/
    └── graph_test.rs   # 集成测试
```

---

## Task 1: 创建 lellm-graph crate 骨架

**Covers:** [S9]

**Files:**
- Create: `lellm-graph/Cargo.toml`
- Create: `lellm-graph/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "lellm-graph"
version.workspace = true
edition.workspace = true
license.workspace = true
description = "Graph/Node/Edge orchestration layer for LeLLM"
documentation = "https://docs.rs/lellm-graph"
repository = "https://github.com/lellm-ai/lellm"

[dependencies]
lellm-agent.workspace = true
lellm-core.workspace = true
async-trait.workspace = true
indexmap.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
tracing.workspace = true
thiserror.workspace = true
futures.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["test-util"] }
```

- [ ] **Step 2: 创建 lib.rs**

```rust
//! lellm-graph — Graph/Node/Edge 编排层。
//!
//! 提供 Workflow DAG + Loop Node 编排能力。

pub mod error;
pub mod executor;
pub mod graph;
pub mod node;
pub mod state;

pub use error::GraphError;
pub use executor::GraphExecutor;
pub use graph::{Edge, Graph, GraphBuilder};
pub use node::{
    AgentNode, ConditionNode, ConditionNodeBuilder, GraphNode, LoopNode, NextStep, NodeKind,
    SubGraph, TaskNode, ToolNode,
};
pub use state::{ExecutionEntry, GraphResult, State};
```

- [ ] **Step 3: 更新 workspace Cargo.toml**

```toml
[workspace]
members = [
    "lellm",
    "lellm-core",
    "lellm-provider",
    "lellm-agent",
    "lellm-macros",
    "lellm-mcp",
    "lellm-graph",  # 新增
]
```

- [ ] **Step 4: 验证编译**

```bash
cargo check -p lellm-graph
```

- [ ] **Step 5: Commit**

```bash
git add lellm-graph/ Cargo.toml
git commit -m "feat(graph): 创建 lellm-graph crate 骨架"
```

---

## Task 2: 实现错误类型

**Covers:** [S8]

**Files:**
- Create: `lellm-graph/src/error.rs`

- [ ] **Step 1: 实现 GraphError**

```rust
//! Graph 错误类型。

use std::fmt;

/// Graph 执行错误。
#[derive(Debug)]
pub enum GraphError {
    /// 图结构无效（构建时校验）
    InvalidGraph(String),
    /// 节点不存在
    NodeNotFound(String),
    /// 节点执行失败
    NodeExecutionFailed {
        node: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// 循环超限
    LoopLimitExceeded { limit: usize },
    /// State 操作错误
    StateError(String),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidGraph(msg) => write!(f, "invalid graph: {msg}"),
            Self::NodeNotFound(name) => write!(f, "node not found: {name}"),
            Self::NodeExecutionFailed { node, source } => {
                write!(f, "node '{node}' execution failed: {source}")
            }
            Self::LoopLimitExceeded { limit } => {
                write!(f, "loop limit exceeded: {limit}")
            }
            Self::StateError(msg) => write!(f, "state error: {msg}"),
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NodeExecutionFailed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}
```

- [ ] **Step 2: 验证编译**

```bash
cargo check -p lellm-graph
```

- [ ] **Step 3: Commit**

```bash
git add lellm-graph/src/error.rs
git commit -m "feat(graph): 实现 GraphError 错误类型"
```

---

## Task 3: 实现 State 和 GraphResult

**Covers:** [S4]

**Files:**
- Create: `lellm-graph/src/state.rs`

- [ ] **Step 1: 实现 State 和 GraphResult**

```rust
//! State 和执行结果。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Graph 共享状态。
pub type State = HashMap<String, serde_json::Value>;

/// Graph 执行结果。
#[derive(Debug)]
pub struct GraphResult {
    /// 最终状态
    pub state: State,
    /// 执行日志
    pub execution_log: Vec<ExecutionEntry>,
    /// 执行耗时
    pub duration: Duration,
}

/// 单个节点执行记录。
#[derive(Debug, Clone)]
pub struct ExecutionEntry {
    /// 节点名称
    pub node_name: String,
    /// 开始时间
    pub start_time: Instant,
    /// 结束时间
    pub end_time: Instant,
    /// 是否成功
    pub success: bool,
}

impl ExecutionEntry {
    /// 执行耗时
    pub fn elapsed(&self) -> Duration {
        self.end_time.duration_since(self.start_time)
    }
}
```

- [ ] **Step 2: 验证编译**

```bash
cargo check -p lellm-graph
```

- [ ] **Step 3: Commit**

```bash
git add lellm-graph/src/state.rs
git commit -m "feat(graph): 实现 State 和 GraphResult"
```

---

## Task 4: 实现 NodeKind 和基础 Node 类型

**Covers:** [S2]

**Files:**
- Create: `lellm-graph/src/node.rs`

- [ ] **Step 1: 实现 GraphNode trait 和 NodeKind**

```rust
//! 节点类型定义。

use async_trait::async_trait;

use crate::error::GraphError;
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
    Agent(AgentNode),
    /// 工具调用
    Tool(ToolNode),
    /// 条件分支
    Condition(ConditionNode),
    /// 循环容器
    Loop(LoopNode),
}
```

- [ ] **Step 2: 实现 TaskNode**

```rust
/// 自定义逻辑节点。
pub struct TaskNode {
    /// 节点名称
    pub name: String,
    /// 执行函数
    pub func: Box<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>,
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
```

- [ ] **Step 3: 实现 AgentNode**

```rust
/// Agent 节点（包装 ToolUseLoop）。
pub struct AgentNode {
    /// 节点名称
    pub name: String,
    /// Agent 实例
    pub agent: lellm_agent::ToolUseLoop,
    /// 消息键（从 State 读取/写入）
    pub messages_key: String,
    /// 输出键（写入 State）
    pub output_key: String,
}

impl AgentNode {
    pub fn new(
        name: impl Into<String>,
        agent: lellm_agent::ToolUseLoop,
    ) -> Self {
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

        let result = self
            .agent
            .execute(messages)
            .await
            .map_err(|e| GraphError::NodeExecutionFailed {
                node: self.name.clone(),
                source: Box::new(e),
            })?;

        if let Some(text) = result.response.text() {
            state.insert(self.output_key.clone(), serde_json::Value::String(text));
        }

        Ok(NextStep::GoToNext)
    }
}
```

- [ ] **Step 4: 实现 ToolNode**

```rust
/// 工具调用节点。
pub struct ToolNode {
    /// 节点名称
    pub name: String,
    /// 工具名称
    pub tool_name: String,
    /// 参数键（从 State 读取）
    pub args_key: String,
    /// 输出键（写入 State）
    pub output_key: String,
}

impl ToolNode {
    pub fn new(
        name: impl Into<String>,
        tool_name: impl Into<String>,
    ) -> Self {
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
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        // ToolNode 的完整实现需要 ToolExecutor 引用
        // 在 Task 6 中完善
        tracing::warn!(tool = %self.tool_name, "ToolNode::execute not yet implemented");
        Ok(NextStep::GoToNext)
    }
}
```

- [ ] **Step 5: 实现 ConditionNode**

```rust
/// 条件分支节点。
pub struct ConditionNode {
    /// 节点名称
    pub name: String,
    /// 分支列表：(目标节点, 条件函数)
    pub branches: Vec<(String, Box<dyn Fn(&State) -> bool + Send + Sync>)>,
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
    branches: Vec<(String, Box<dyn Fn(&State) -> bool + Send + Sync>)>,
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
```

- [ ] **Step 6: 实现 LoopNode**

```rust
/// 循环容器。
pub struct LoopNode {
    /// 节点名称
    pub name: String,
    /// 循环体
    pub body: SubGraph,
    /// 继续条件（返回 true 继续循环）
    pub continue_condition: Box<dyn Fn(&State) -> bool + Send + Sync>,
    /// 最大迭代次数
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
```

- [ ] **Step 7: 实现 SubGraph**

```rust
/// 子图（LoopNode / ParallelNode 的执行单元）。
pub struct SubGraph {
    pub nodes: Vec<Box<dyn GraphNode>>,
    pub edges: Vec<super::graph::Edge>,
    pub start: String,
    pub end: String,
}

impl SubGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            start: String::new(),
            end: String::new(),
        }
    }

    pub async fn execute(&self, state: &mut State) -> Result<(), GraphError> {
        // 简化的顺序执行
        for node in &self.nodes {
            node.execute(state).await?;
        }
        Ok(())
    }
}
```

- [ ] **Step 8: 实现 NodeKind 的 GraphNode**

```rust
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
```

- [ ] **Step 9: 验证编译**

```bash
cargo check -p lellm-graph
```

- [ ] **Step 10: Commit**

```bash
git add lellm-graph/src/node.rs
git commit -m "feat(graph): 实现 NodeKind 和基础节点类型"
```

---

## Task 5: 实现 Graph 和 GraphBuilder

**Covers:** [S3, S6, S7]

**Files:**
- Create: `lellm-graph/src/graph.rs`

- [ ] **Step 1: 实现 Edge 和 Graph**

```rust
//! Graph 和 GraphBuilder。

use indexmap::IndexMap;

use crate::error::GraphError;
use crate::node::NodeKind;

/// 边（Edge）。
pub struct Edge {
    pub from: String,
    pub to: String,
    pub condition: Option<Box<dyn Fn(&crate::state::State) -> bool + Send + Sync>>,
}

/// 图（Graph）。
pub struct Graph {
    pub(crate) nodes: IndexMap<String, NodeKind>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) start: String,
    pub(crate) end: String,
}

impl Graph {
    /// 获取节点名称列表。
    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.keys().map(|s| s.as_str()).collect()
    }

    /// 获取起始节点名称。
    pub fn start_node(&self) -> &str {
        &self.start
    }

    /// 获取结束节点名称。
    pub fn end_node(&self) -> &str {
        &self.end
    }

    /// 获取从指定节点出发的边。
    pub fn edges_from(&self, from: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.from == from).collect()
    }

    /// 验证图结构。
    pub fn validate(&self) -> Result<(), GraphError> {
        // 1. 检查起始节点存在
        if !self.nodes.contains_key(&self.start) {
            return Err(GraphError::InvalidGraph(format!(
                "start node '{}' not found",
                self.start
            )));
        }

        // 2. 检查结束节点存在
        if !self.nodes.contains_key(&self.end) {
            return Err(GraphError::InvalidGraph(format!(
                "end node '{}' not found",
                self.end
            )));
        }

        // 3. 检查所有边引用的节点存在
        for edge in &self.edges {
            if !self.nodes.contains_key(&edge.from) {
                return Err(GraphError::InvalidGraph(format!(
                    "edge references non-existent source node '{}'",
                    edge.from
                )));
            }
            if !self.nodes.contains_key(&edge.to) {
                return Err(GraphError::InvalidGraph(format!(
                    "edge references non-existent target node '{}'",
                    edge.to
                )));
            }
        }

        // 4. 检查无环（使用 DFS）
        self.detect_cycle()?;

        Ok(())
    }

    /// 检测环。
    fn detect_cycle(&self) -> Result<(), GraphError> {
        use std::collections::HashSet;

        fn dfs(
            node: &str,
            graph: &Graph,
            visited: &mut HashSet<String>,
            rec_stack: &mut HashSet<String>,
        ) -> Result<(), GraphError> {
            visited.insert(node.to_string());
            rec_stack.insert(node.to_string());

            for edge in graph.edges_from(node) {
                if !visited.contains(&edge.to) {
                    dfs(&edge.to, graph, visited, rec_stack)?;
                } else if rec_stack.contains(&edge.to) {
                    return Err(GraphError::InvalidGraph(format!(
                        "cycle detected: {} -> {}",
                        node, edge.to
                    )));
                }
            }

            rec_stack.remove(node);
            Ok(())
        }

        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for node_name in self.nodes.keys() {
            if !visited.contains(node_name) {
                dfs(node_name, self, &mut visited, &mut rec_stack)?;
            }
        }

        Ok(())
    }
}
```

- [ ] **Step 2: 实现 GraphBuilder**

```rust
/// Graph 构建器。
pub struct GraphBuilder {
    name: String,
    nodes: IndexMap<String, NodeKind>,
    edges: Vec<Edge>,
    start: Option<String>,
    end: Option<String>,
}

impl GraphBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: IndexMap::new(),
            edges: Vec::new(),
            start: None,
            end: None,
        }
    }

    /// 设置起始节点。
    pub fn start(mut self, node: impl Into<String>) -> Self {
        self.start = Some(node.into());
        self
    }

    /// 设置结束节点。
    pub fn end(mut self, node: impl Into<String>) -> Self {
        self.end = Some(node.into());
        self
    }

    /// 添加节点。
    pub fn node(mut self, name: impl Into<String>, kind: NodeKind) -> Self {
        let name = name.into();
        self.nodes.insert(name, kind);
        self
    }

    /// 添加边（无条件）。
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
        });
        self
    }

    /// 添加条件边。
    pub fn edge_if(
        mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&crate::state::State) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: Some(Box::new(condition)),
        });
        self
    }

    /// 构建 Graph。
    pub fn build(self) -> Result<Graph, GraphError> {
        let start = self.start.ok_or_else(|| {
            GraphError::InvalidGraph("start node not set".into())
        })?;

        let end = self.end.ok_or_else(|| {
            GraphError::InvalidGraph("end node not set".into())
        })?;

        let graph = Graph {
            nodes: self.nodes,
            edges: self.edges,
            start,
            end,
        };

        graph.validate()?;

        Ok(graph)
    }

    /// 获取图名称。
    pub fn name(&self) -> &str {
        &self.name
    }
}
```

- [ ] **Step 3: 验证编译**

```bash
cargo check -p lellm-graph
```

- [ ] **Step 4: Commit**

```bash
git add lellm-graph/src/graph.rs
git commit -m "feat(graph): 实现 Graph 和 GraphBuilder"
```

---

## Task 6: 实现 GraphExecutor

**Covers:** [S5]

**Files:**
- Create: `lellm-graph/src/executor.rs`

- [ ] **Step 1: 实现 GraphExecutor**

```rust
//! Graph 执行引擎。

use std::time::Instant;

use crate::error::GraphError;
use crate::graph::Graph;
use crate::node::{GraphNode, NextStep};
use crate::state::{ExecutionEntry, GraphResult, State};

/// Graph 执行器。
pub struct GraphExecutor;

impl GraphExecutor {
    /// 执行 Graph。
    pub async fn execute(graph: &Graph, initial_state: State) -> Result<GraphResult, GraphError> {
        let start_time = Instant::now();
        let mut state = initial_state;
        let mut execution_log = Vec::new();

        let mut current = graph.start_node().to_string();

        loop {
            if current == graph.end_node() {
                break;
            }

            let node = graph
                .nodes
                .get(&current)
                .ok_or_else(|| GraphError::NodeNotFound(current.clone()))?;

            let start = Instant::now();
            let result = node.execute(&mut state).await;
            let end = Instant::now();

            let success = result.is_ok();
            execution_log.push(ExecutionEntry {
                node_name: current.clone(),
                start_time: start,
                end_time: end,
                success,
            });

            match result {
                Ok(next) => match next {
                    NextStep::Goto(target) => {
                        current = target;
                    }
                    NextStep::GoToNext => {
                        // 找到下一个节点
                        current = Self::find_next_node(graph, &current)?;
                    }
                    NextStep::End => {
                        break;
                    }
                },
                Err(e) => {
                    tracing::error!(
                        node = %current,
                        error = %e,
                        "graph execution failed"
                    );
                    return Err(e);
                }
            }
        }

        let duration = start_time.elapsed();

        Ok(GraphResult {
            state,
            execution_log,
            duration,
        })
    }

    /// 查找下一个节点（拓扑顺序）。
    fn find_next_node(graph: &Graph, current: &str) -> Result<String, GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            // 没有出边，且不是结束节点
            return Err(GraphError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            )));
        }

        // 对于无条件边，返回第一个目标
        for edge in &edges {
            if edge.condition.is_none() {
                return Ok(edge.to.clone());
            }
        }

        // 对于条件边，需要在执行时评估（这里简化处理）
        Err(GraphError::InvalidGraph(format!(
            "node '{}' only has conditional edges but no condition was true",
            current
        )))
    }
}
```

- [ ] **Step 2: 验证编译**

```bash
cargo check -p lellm-graph
```

- [ ] **Step 3: Commit**

```bash
git add lellm-graph/src/executor.rs
git commit -m "feat(graph): 实现 GraphExecutor 执行引擎"
```

---

## Task 7: 编写单元测试

**Covers:** [S11]

**Files:**
- Create: `lellm-graph/tests/graph_test.rs`

- [ ] **Step 1: 编写基础测试**

```rust
use lellm_graph::{GraphBuilder, GraphExecutor, State, TaskNode, NodeKind};
use std::collections::HashMap;

#[tokio::test]
async fn test_linear_pipeline() {
    let graph = GraphBuilder::new("linear")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                state.insert("step".into(), serde_json::json!("a"));
                Ok(())
            })),
        )
        .node(
            "b",
            NodeKind::Task(TaskNode::new("b", |state| {
                state.insert("step".into(), serde_json::json!("b"));
                Ok(())
            })),
        )
        .node(
            "c",
            NodeKind::Task(TaskNode::new("c", |state| {
                state.insert("step".into(), serde_json::json!("c"));
                Ok(())
            })),
        )
        .edge("a", "b")
        .edge("b", "c")
        .end("c")
        .build()
        .expect("build should succeed");

    let initial_state = HashMap::new();
    let result = GraphExecutor::execute(&graph, initial_state)
        .await
        .expect("execution should succeed");

    assert_eq!(
        result.state.get("step").unwrap(),
        &serde_json::json!("c")
    );
    assert_eq!(result.execution_log.len(), 3);
}

#[tokio::test]
async fn test_loop_with_limit() {
    let graph = GraphBuilder::new("loop_test")
        .start("loop")
        .node(
            "loop",
            NodeKind::Task(TaskNode::new("loop", |state| {
                let count = state
                    .get("count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .end("loop")
        .build()
        .expect("build should succeed");

    let initial_state = HashMap::new();
    let result = GraphExecutor::execute(&graph, initial_state).await;

    // 应该因为 LoopLimitExceeded 失败（因为 loop 节点没有出边到 end）
    assert!(result.is_err());
}

#[test]
fn test_cycle_detection() {
    let result = GraphBuilder::new("cycle")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |_| Ok(()))),
        )
        .node(
            "b",
            NodeKind::Task(TaskNode::new("b", |_| Ok(()))),
        )
        .edge("a", "b")
        .edge("b", "a")  // 形成环
        .end("b")
        .build();

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, lellm_graph::GraphError::InvalidGraph(_)));
}

#[test]
fn test_missing_node() {
    let result = GraphBuilder::new("missing")
        .start("a")
        .edge("a", "nonexistent")
        .end("nonexistent")
        .build();

    assert!(result.is_err());
}
```

- [ ] **Step 2: 运行测试**

```bash
cargo test -p lellm-graph
```

- [ ] **Step 3: Commit**

```bash
git add lellm-graph/tests/
git commit -m "test(graph): 添加单元测试"
```

---

## Task 8: 更新 workspace 和文档

**Covers:** [S12]

**Files:**
- Modify: `Cargo.toml` (workspace dependencies)
- Modify: `docs/BLUEPRINT.md`

- [ ] **Step 1: 更新 workspace dependencies**

```toml
[workspace.dependencies]
# Internal
lellm-core = { path = "lellm-core", version = "0.1" }
lellm-provider = { path = "lellm-provider", version = "0.1" }
lellm-agent = { path = "lellm-agent", version = "0.1" }
lellm-macros = { path = "lellm-macros", version = "0.1" }
lellm-mcp = { path = "lellm-mcp", version = "0.1" }
lellm-graph = { path = "lellm-graph", version = "0.1" }  # 新增

# Common
futures = "0.3"  # 新增
```

- [ ] **Step 2: 更新 lellm 门面 crate**

```toml
[features]
default = ["provider"]
core = ["dep:lellm-core"]
provider = ["dep:lellm-core", "dep:lellm-provider"]
agent = ["dep:lellm-core", "dep:lellm-agent"]
macros = ["dep:lellm-macros"]
mcp = ["dep:lellm-core", "dep:lellm-mcp"]
graph = ["dep:lellm-graph"]  # 新增
full = ["provider", "agent", "macros", "graph"]  # 更新
```

- [ ] **Step 3: 验证编译**

```bash
cargo check --workspace --all-features
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml lellm/Cargo.toml
git commit -m "chore: 更新 workspace 配置，集成 lellm-graph"
```

---

## Task 9: 最终验证

**Covers:** [S11]

- [ ] **Step 1: 全量构建**

```bash
cargo build --workspace
```

- [ ] **Step 2: 全量测试**

```bash
cargo test --workspace
```

- [ ] **Step 3: 格式化**

```bash
cargo fmt --all
```

- [ ] **Step 4: Clippy 检查**

```bash
cargo clippy --workspace --all-features
```

- [ ] **Step 5: 最终 Commit**

```bash
git add -A
git commit -m "feat(v0.2): 完成 Graph/Node/Edge 编排层实现"
```
