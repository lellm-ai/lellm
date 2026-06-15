# LeLLM v0.2 Graph/Node/Edge 编排层设计

> 版本：v0.2 | 日期：2026-06-15 | 状态：代码已实现，文档追平代码
>
> **原则：** 本文档以实际代码为准。设计文档与代码的差距分析见 [v02-graph-design-diff.md](./v02-graph-design-diff.md)

---

## [S1] 设计目标

**有环图 + 熔断器** — 图结构允许任意环，循环保护由 `GraphExecutor::max_steps` 运行时熔断提供。

### 核心约束

| 约束 | 决策 |
|------|------|
| 图类型 | **允许有环**，A→B→C→A 完全合法 |
| 循环保护 | `GraphExecutor::max_steps` 全局熔断（默认 50 步）+ `LoopNode::max_iterations` 局部熔断 |
| 控制流 | Sequence, Condition (edge_if), Parallel (未实现), Loop |
| Node 种类 | 7 种：Task, Agent, LLM, Tool, Condition, Loop, Barrier |
| 数据传递 | 共享 State（`HashMap<String, Value>`）+ Reducer 合并机制 |
| 执行模式 | 宏观串行，节点依次执行 |
| 流式支持 | `execute_stream()` 返回 `GraphStream`，实时发射 `GraphEvent` |

### 路线 B 决策

原始设计承诺"严格 DAG + LoopNode 表达循环"，实现时选择了**路线 B**：

- **有环图** — `edge_if` 天然支持回跳，无需 ConditionNode 中转
- **熔断器** — `max_steps` 全局步数限制，防止无限循环
- **LoopNode 保留** — 作为需要独立迭代计数和独立熔断的语法糖

```rust
// 推荐：直接用有环图 + edge_if（更直观）
GraphBuilder::new("retry")
    .edge_if("check", "agent", |s| !s.satisfied)  // 回跳
    .edge("check", "output")                       // 通过

// LoopNode：需要独立 max_iterations 时使用
LoopNode::new("loop", SubGraph { ... }, |s| !s.satisfied, max_iterations: 5)
```

---

## [S2] Node 类型定义

```rust
pub enum NodeKind {
    Task(TaskNode),                    // 自定义逻辑
    Agent(Box<AgentNode>),             // 包装 ToolUseLoop（完整 ReAct 循环）
    LLM(LLMNode),                      // 单次 LLM 调用（手动 ReAct）
    Tool(ToolNode),                    // 工具调用
    Condition(ConditionNode),          // 条件分支
    Loop(Box<LoopNode>),              // 循环容器（语法糖）
    Barrier(BarrierNode),              // Human-in-the-loop 审批
}
```

> **注意：** Agent 和 Loop 用 `Box` 包裹，因为体积不确定（AgentNode 含 ToolUseLoop，LoopNode 含 SubGraph）。

### TaskNode — 自定义逻辑

```rust
pub struct TaskNode {
    pub name: String,
    pub func: Box<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>,
}
```

始终返回 `NextStep::GoToNext`。

### AgentNode — 完整 ReAct 循环

```rust
pub struct AgentNode {
    pub name: String,
    pub agent: lellm_agent::ToolUseLoop,
    pub prefix: String,          // State key 前缀，默认 "agent"
    pub write_messages: bool,    // 是否写回完整 messages，默认 true
    pub write_stats: bool,       // 是否写回执行统计，默认 true
}
```

执行后自动写回 State：
- `{prefix}.messages` — 完整对话历史
- `{prefix}.output` — 最终回复纯文本
- `{prefix}.iterations` — LLM 调用轮次
- `{prefix}.tool_calls` — 工具调用总数
- `{prefix}.stop_reason` — 停止原因

### LLMNode — 单次 LLM 调用

```rust
pub struct LLMNode {
    pub name: String,
    model: lellm_agent::ResolvedModel,
    system_prompt: Option<String>,
    messages_key: String,  // 默认 "messages"
}
```

与 `AgentNode`（完整 ReAct 循环）不同，`LLMNode` 仅执行一次 LLM 调用，将响应追加到消息列表。

**用途：** 配合 `ToolNode` + `ConditionNode` 手动构建 ReAct 循环。

**警告：** 使用 LLMNode 手动构建循环时，会失去 `AgentNode` 提供的保护（ParallelSafety、RetryPolicy、FallbackStrategy、预算保险丝、Context Compaction）。除非有明确理由，否则请使用 `AgentNode`。

### ToolNode — 工具执行

```rust
pub struct ToolNode {
    pub name: String,
    executor: lellm_agent::ToolExecutor,  // 直接持有 ToolExecutor
    messages_key: String,                 // 默认 "messages"
}
```

读取 State 中最后一条 Assistant 消息的 `tool_calls`，执行所有工具调用，将 `ToolResult` 消息追加到消息列表。

### ConditionNode — 条件分支

```rust
pub struct ConditionNode {
    pub name: String,
    pub branches: Vec<(String, Box<dyn Fn(&State) -> bool + Send + Sync>)>,
}
```

按声明顺序求值分支条件，返回第一个匹配分支的 `NextStep::Goto(target)`。无匹配时报错。

提供 Builder：

```rust
ConditionNode::builder("route")
    .branch("retry", |s| s.get("valid").map(|v| v.as_bool() == Some(false)).unwrap_or(false))
    .branch("done", |_| true)  // fallback
    .build()
```

### LoopNode — 循环容器

```rust
pub struct LoopNode {
    pub name: String,
    pub body: SubGraph,
    pub continue_condition: Box<dyn Fn(&State) -> bool + Send + Sync>,
    pub max_iterations: usize,
}
```

- 每次执行完 body 后求值 `continue_condition`
- 条件为 false 时退出，返回 `GoToNext`
- 达到 `max_iterations` 时返回 `LoopLimitExceeded` 错误
- 独立于全局 `max_steps`，提供局部熔断

### BarrierNode — Human-in-the-loop 审批

```rust
pub struct BarrierNode {
    pub name: String,
    pub timeout: Option<Duration>,           // 超时时间（None = 无限等待）
    pub default_action: BarrierDefaultAction, // 超时默认行为
    pub reject_key: String,                  // 拒绝原因 key
    pub approve_key: String,                 // 审批通过 key
}
```

**仅支持流式模式。** 阻塞模式直接报错，引导使用 `execute_stream()`。

执行流程：
1. 发射 `GraphEvent::BarrierPaused { signal }` 到 sink
2. `tokio::select!` 等待决策信号或超时
3. 根据决策写入 State，决定下一步

四种决策：

```rust
pub enum BarrierDecision {
    Approve,                    // 通过 — 继续下一步
    Reject { reason: String },  // 拒绝 — 写入拒绝原因，由 edge_if 决定是否回跳
    Modify { key, value },      // 修改 State 指定 key，然后继续
    Reroute { target },         // 跳转到指定节点
}
```

超时默认行为：

```rust
pub enum BarrierDefaultAction {
    Reject,  // 超时视为拒绝（默认）
    Approve, // 超时视为通过
    Skip,    // 超时跳过（继续下一步）
}
```

### SubGraph — 子图

```rust
pub struct SubGraph {
    pub nodes: Vec<Box<dyn GraphNode>>,
    pub edges: Vec<Edge>,
}
```

LoopNode 的执行单元，线性执行所有节点。
- `GoToNext` — 继续遍历下一个节点
- `End` — 提前退出子图
- `Goto(target)` — 报错（SubGraph 不支持按名跳转）

### GraphNode Trait

```rust
#[async_trait]
pub trait GraphNode: Send + Sync {
    /// 阻塞执行
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError>;

    /// 流式执行，将内部事件转发到 channel
    /// 默认实现直接调用 execute，不产生流式事件
    async fn execute_stream(
        &self,
        state: &mut State,
        sink: &tokio::sync::mpsc::Sender<GraphEvent>,
    ) -> Result<NextStep, GraphError>;
}
```

---

## [S3] Graph 结构

```rust
pub struct Graph {
    pub(crate) nodes: IndexMap<String, NodeKind>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) start: String,
    pub(crate) end: String,
}

pub struct Edge {
    pub from: String,
    pub to: String,
    pub condition: Option<Box<dyn Fn(&State) -> bool + Send + Sync>>,
}
```

**图允许有环。** 循环保护由 `GraphExecutor::max_steps` 运行时熔断提供。

### 验证规则

`validate()` 检查：
1. 起始节点存在
2. 结束节点存在
3. 所有边引用的节点存在

**不检查：**
- 循环检测（有环图不需要）
- 节点可达性（未实现）
- 条件边覆盖完整性（未实现）

---

## [S4] State 设计

### 基础类型

```rust
pub type State = HashMap<String, serde_json::Value>;
```

### Reducer 机制

```rust
/// 将已有值与新值合并
pub type StateReducer = Box<
    dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync,
>;

pub trait StateExt {
    fn reduce(&mut self, key: &str, value: Value, reducer: &StateReducer) -> Result<(), String>;
    fn append_array(&mut self, key: &str, items: Value) -> Result<(), String>;
}

/// 内置：数组追加（类似 LangGraph 的 operator.add for lists）
pub fn array_reducer() -> StateReducer;
```

### 执行结果

```rust
pub struct GraphResult {
    pub state: State,
    pub execution_log: Vec<ExecutionEntry>,
    pub duration: Duration,
}

pub struct ExecutionEntry {
    pub node_name: String,
    pub start_time: Instant,
    pub end_time: Instant,
    pub success: bool,
}
```

---

## [S5] 执行语义

### GraphExecutor

```rust
pub struct GraphExecutor {
    pub max_steps: usize,  // 全局步数限制，默认 50
}
```

两种执行模式：

| 模式 | 方法 | 返回 | 适用场景 |
|------|------|------|---------|
| 阻塞 | `execute(&graph, state)` | `Result<GraphResult, GraphError>` | 简单流水线、测试 |
| 流式 | `execute_stream(graph, state)` | `GraphStream` | 需要实时事件、BarrierNode |

### 执行流程（阻塞模式）

```
current = start_node
loop {
  if step > max_steps → StepsExceeded 熔断
  if current == end_node → break
  result = node.execute(state)
  match result {
    Goto(target)   → current = target
    GoToNext       → current = find_next_node(graph, current, state)
    End            → break
    Err(e)         → 立即返回错误（fail-fast）
  }
}
```

### find_next_node 优先级

```rust
// 1. 先评估条件边（按声明顺序，返回第一个为 true 的目标）
for edge in edges {
    if edge.condition.is_some() && condition(state) {
        return Ok(edge.to);
    }
}

// 2. 无条件边（默认 fallback）
for edge in edges {
    if edge.condition.is_none() {
        return Ok(edge.to);
    }
}

// 3. 都不匹配 → 报错（附带所有条件的评估结果）
```

### 流式执行

`execute_stream()` 返回 `GraphStream`（`mpsc::Receiver<GraphEvent>`），消费者实时接收：

```rust
pub enum GraphEvent {
    NodeStart { node_name },
    NodeEnd { node_name, success, duration },
    Agent { node_name, event: AgentEvent },    // Agent 内部事件穿透
    BarrierPaused { node_name, signal },        // 等待外部审批
    GraphComplete { result: GraphResult },      // 恰好一次
    GraphError { error: GraphError },           // 恰好一次
}
```

**生命周期契约：**
- 正常结束：`GraphComplete` 恰好一次，然后 channel 关闭
- 异常结束：`GraphError` 恰好一次，然后 channel 关闭
- 终态事件后不再发送任何事件

### 错误处理

**Fail-Fast：** 节点失败立即停止，返回错误。

---

## [S6] Builder API

```rust
let graph = GraphBuilder::new("workflow")
    .start("init")
    .node("init", NodeKind::Task(TaskNode::new("init", |state| { ... })))
    .node("agent", NodeKind::Agent(Box::new(AgentNode::new("agent", agent)
        .with_prefix("calc"))))
    .node("check", NodeKind::Condition(
        ConditionNode::builder("check")
            .branch("retry", |s| !s.satisfied())
            .branch("done", |_| true)
            .build()
    ))
    .edge("init", "agent")
    .edge_if("agent", "check", |s| s.has_tool_calls())  // 条件边
    .edge("agent", "done")                                // 无条件 fallback
    .end("done")
    .build()?;
```

### GraphBuilder 方法

| 方法 | 说明 |
|------|------|
| `new(name)` | 创建构建器 |
| `start(node)` | 设置起始节点 |
| `end(node)` | 设置结束节点 |
| `node(name, kind)` | 添加节点 |
| `edge(from, to)` | 添加无条件边 |
| `edge_if(from, to, condition)` | 添加条件边 |
| `build()` | 构建并验证，返回 `Result<Graph, GraphError>` |

---

## [S7] 错误类型

```rust
pub enum GraphError {
    InvalidGraph(String),                                    // 图结构无效
    NodeNotFound(String),                                    // 节点不存在
    NodeExecutionFailed { node: String, source: Box<dyn Error + Send + Sync> },
    LoopLimitExceeded { limit: usize },                      // LoopNode 局部超限
    StepsExceeded { limit: usize },                          // 全局步数超限（熔断）
    BarrierTimeout { node: String, timeout: Duration },      // Barrier 超时
    BarrierCancelled { node: String },                       // Barrier 被取消
    StateError(String),                                      // State 操作错误
}
```

---

## [S8] Crate 结构

```
lellm-graph/
├── Cargo.toml
├── examples/
│   └── calculator_graph.rs       # LangGraph Tutorial 对照实现
├── tests/
│   └── graph_test.rs             # 集成测试
└── src/
    ├── lib.rs                    # 公开 API
    ├── error.rs                  # GraphError
    ├── state.rs                  # State, GraphResult, ExecutionEntry, StateReducer
    ├── node.rs                   # GraphNode trait, NextStep, NodeKind
    │                              # TaskNode, ConditionNode, LoopNode, SubGraph
    ├── llm_node.rs               # AgentNode, LLMNode
    ├── tool_node.rs              # ToolNode
    ├── barrier_node.rs           # BarrierNode, BarrierDecision
    ├── event.rs                  # GraphEvent, GraphStream
    ├── graph.rs                  # Graph, Edge, GraphBuilder
    └── executor.rs               # GraphExecutor（阻塞 + 流式）
```

---

## [S9] 测试覆盖

| 场景 | 测试 | 状态 |
|------|------|------|
| 线性流水线 | `test_linear_pipeline` | ✅ |
| 条件分支 | `test_condition_branching` | ✅ |
| 节点错误 | `test_task_node_error` | ✅ |
| 有环图构建 | `test_cyclic_graph_allowed` | ✅ |
| 有环图熔断 | `test_cyclic_graph_steps_exceeded` | ✅ |
| 有环图 + edge_if 退出 | `test_cyclic_graph_with_edge_if_exit` | ✅ |
| ConditionNode 回跳 | `test_condition_node_back_jump` | ✅ |
| LoopNode 基本 | `test_loop_node_basic` | ✅ |
| LoopNode 超限 | `test_loop_node_limit_exceeded` | ✅ |
| Barrier 阻塞模式报错 | `test_barrier_blocked_mode_error` | ✅ |
| Barrier Approve | `test_barrier_approve` | ✅ |
| Barrier Reject + 回跳 | `test_barrier_reject_with_back_jump` | ✅ |
| Barrier Modify | `test_barrier_modify` | ✅ |
| Barrier 超时 | `test_barrier_timeout` | ✅ |
| Barrier Reroute | `test_barrier_reroute` | ✅ |
| 执行日志 | `test_execution_log` | ✅ |
| 缺失节点 | `test_missing_node/start/end` | ✅ |

---

## [S10] 待实现

| 优先级 | 功能 | 说明 |
|--------|------|------|
| P1 | ParallelNode | 并行子图，`join_all` + Reducer 聚合 |
| P2 | 可达性验证 | validate() 检查所有节点是否可达 |
| P3 | 条件覆盖验证 | validate() 检查条件边是否覆盖完整 |

---

## [S11] 版本路线图

| 版本 | 范围 | 状态 |
|------|------|------|
| **v0.2** | Graph/Node/Edge + 有环图 + BarrierNode + 流式执行 | ✅ 已完成 |
| **v0.3** | ParallelNode + Checkpoint + 持久化 | 规划中 |
| **v0.4** | Multi-Agent Orchestration + Agent-to-Agent via MCP | 规划中 |

> **注意：** 原始路线图的 v0.3"StateGraph（任意环）"已被 v0.2 路线 B 覆盖——有环图已是 v0.2 的核心特性。

---

## [S12] 与 v0.1 集成

- `AgentNode` 直接持有 `ToolUseLoop`，复用完整的 ReAct 循环
- `LLMNode` 持有 `ResolvedModel`，单次调用 LLM
- `ToolNode` 直接持有 `ToolExecutor`
- 复用 `LlmError`, `Message`, `ContentBlock` 等核心类型
- `LoopDetector`/`SignalVoter` 在 `v02-preview` feature gate 后，未默认开启
