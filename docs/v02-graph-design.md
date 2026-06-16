# LeLLM v0.2 Graph/Node/Edge 编排层设计

> 版本：v0.2 | 日期：2026-06-16 | 状态：代码已实现，文档同步代码
>
> **原则：** 本文档以实际代码为准。

---

## [S1] 设计目标

**有环图 + 熔断器** — 图结构允许任意环，循环保护由 `GraphExecutor::max_steps` 运行时熔断提供。

### 核心约束

| 约束 | 决策 |
|------|------|
| 图类型 | **允许有环**，A→B→C→A 完全合法 |
| 循环保护 | **两层体系**（详见下文）|
| 控制流 | Sequence, Condition (edge_if), Parallel (未实现) |
| Node 种类 | 5 种：Task, Agent, Tool, Condition, Barrier（LLMNode 非 NodeKind 变体）|
| 数据传递 | 共享 State（`HashMap<String, Value>`）+ Reducer 合并机制 |
| 执行模式 | 宏观串行，节点依次执行 |
| 流式支持 | `execute_stream()` 返回 `GraphExecution`（stream + handle），实时发射 `GraphEvent` |

### 循环保护：两层体系

| 层级 | 机制 | 作用域 | 说明 |
|------|------|--------|------|
| **全局** | `GraphExecutor::max_steps` | 整个图执行 | 绝对安全网，默认 50 步 |
| **语法糖** | Loop DSL（Builder 层展开）| 封装循环体 | Builder 展开为 ConditionNode + 边，Runtime 不感知 |

**Step 定义：1 Step = 1 Node Entry**

- 进入 Node 即 +1 step
- Node 内部执行（ReAct / tool / loop）不计 step
- Edge traversal 不单独计 step

```rust
// 推荐：有环图 + edge_if（最直观）
GraphBuilder::new("retry")
    .edge_if("check", "agent", |s| !s.satisfied)?
    .edge("check", "output")?
    .end("output")?

// 循环通过回边表达：
// A → B → C → B（回跳）→ C → B → ... → 输出
```

**设计意图：**
- 全局熔断是**底线**——任何图都不会无限执行
- **循环通过回边表达**——Runtime 只理解一种循环模型
- 未来可通过 Loop DSL（Builder 语法糖）提供更易读的循环表达

---

## [S2] Node 类型定义 — Agent 三层能力模型

| 层级 | 能力 | 用户 | 版本 |
|------|------|------|------|
| Level 1（默认） | `AgentNode` — 黑盒 ReAct | 90% 用户 | v0.2 |
| Level 2（逃生口） | `LLMNode` + `ToolNode` — 手动搭建 ReAct | 高级用户 | v0.2 |
| Level 3（干预） | `AgentHook` — before_tool / after_tool / after_iteration | 需要轻量干预的用户 | v0.3+ |

**核心原则：**
- Graph 不负责表达 Agent 内部 ReAct
- `AgentNode` 是 Graph 中的原子执行单元
- 需要细粒度控制 → `LLMNode` + `ToolNode`
- 需要轻量干预 → `AgentHook`（v0.3+）

### NodeKind 枚举

```rust
pub enum NodeKind {
    Task(TaskNode),                    // 自定义逻辑
    Agent(Box<AgentNode>),             // 包装 ToolUseLoop（完整 ReAct 循环）
    Tool(ToolNode),                    // 工具调用
    Condition(ConditionNode),          // 条件分支
    Barrier(BarrierNode),              // Human-in-the-loop 审批
}
```

> **注意：** Agent 用 `Box` 包裹，因为体积不确定（AgentNode 含 ToolUseLoop）。
> **LLMNode 不是 NodeKind 变体** — 它通过 `llm_node` 模块导出，供手动构建 ReAct 循环使用（Level 2 逃生口）。
> **循环通过回边表达** — 不需要专门的 LoopNode 变体。

**回调类型统一使用 `Arc<dyn Fn>`**（非 `Box<dyn Fn>`），以支持 `Graph` 的 `Clone`。

### TaskNode — 自定义逻辑

```rust
pub type TaskFn = Arc<dyn Fn(&mut State) -> Result<(), GraphError> + Send + Sync>;

pub struct TaskNode {
    pub name: String,
    pub func: TaskFn,
}
```

始终返回 `NextStep::GoToNext`。

### AgentNode — 完整 ReAct 循环

```rust
pub struct AgentNode {
    pub name: String,
    pub agent: lellm_agent::ToolUseLoop,
    /// 业务结果写入 State 的 key（None = 不写入）
    pub output_key: Option<String>,
    /// 对话历史写入 State 的 key（None = 不写入）
    pub messages_key: Option<String>,
    /// 输入消息读取的 State key（默认 "messages"）
    input_key: String,
}
```

**显式声明写入：** AgentNode 默认不写入任何 State。用户通过 builder 方法显式绑定：

```rust
AgentNode::new("planner", agent)
    .with_output("planner.output")        // 业务结果
    .with_messages("planner.messages")     // 对话历史
    .with_input_key("input.messages")      // 输入 key（可选）
```

**执行元数据**（iterations、tool_calls、stop_reason）进入 `ExecutionTrace`，不写入 State。

### LLMNode — 单次 LLM 调用（非 NodeKind，Level 2 逃生口）

```rust
pub struct LLMNode {
    pub name: String,
    model: lellm_agent::ResolvedModel,
    system_prompt: Option<String>,
    messages_key: String,
    tools: Option<Vec<ToolDefinition>>,
}
```

与 `AgentNode`（完整 ReAct 循环）不同，`LLMNode` 仅执行一次 LLM 调用，将响应追加到消息列表。

**用途：** 配合 `ToolNode` + `ConditionNode` 手动构建 ReAct 循环。

```rust
let tools = tool_executor.definitions();
LLMNode::new("llm", model)
    .with_tools(tools)                    // 传入工具定义，LLM 才能返回 tool_calls
    .with_system_prompt("你是一个助手")
    .with_messages_key("planner.messages")
```

**警告：** 使用 LLMNode 手动构建循环时，会失去 `AgentNode` 提供的保护（ParallelSafety、RetryPolicy、FallbackStrategy、预算保险丝、Context Compaction）。除非有明确理由，否则请使用 `AgentNode`。

### ToolNode — 工具执行

```rust
pub struct ToolNode {
    pub name: String,
    executor: lellm_agent::ToolExecutor,  // 直接持有 ToolExecutor
    messages_key: String,                 // 默认 "messages"
}
```

读取 State 中最后一条 Assistant 消息的 `tool_calls`，执行所有工具调用，将 `ToolResult` 消息**追加**到消息列表（不重写整个列表）。

Builder 方法：`ToolNode::all(executor)` — 包含所有注册工具；`ToolNode::new(name, executor)` — 指定名称；`with_messages_key()`。

### ConditionNode — 条件分支

```rust
pub type BranchCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;

pub struct ConditionNode {
    pub name: String,
    pub branches: Vec<(String, BranchCondition)>,
}
```

按声明顺序求值分支条件，返回第一个匹配分支的 `NextStep::Goto(target)`。
无匹配时返回 `NextStep::GoToNext`，由 Graph 层的 `edge_fallback` 处理兜底路由。

**兜底路由统一到 Graph 层：**

```rust
// 节点只声明分支条件
ConditionNode::builder("route")
    .branch("fast_path", |s| s.get("score").map(|v| v.as_u64().unwrap_or(0) >= 80))
    .branch("slow_path", |s| s.get("score").map(|v| v.as_u64().unwrap_or(0) >= 50))
    .build()

// 兜底路由在 Graph 层通过 edge_fallback 定义
g.edge_fallback("route", "default")?;
```

> **为什么不在 ConditionNode 里放 otherwise_target？**
> 节点只负责计算状态，边只负责控制流向。兜底路由是拓扑概念，应在 Graph 层通过 `edge_fallback` 表达，这样 `build()` 时可以验证目标节点是否存在。

### BarrierNode — Human-in-the-loop 审批

```rust
pub struct BarrierNode {
    pub name: String,
    pub timeout: Option<Duration>,           // 超时时间（None = 无限等待）
    pub default_action: BarrierDefaultAction, // 超时默认行为
    pub reject_key: String,                  // 拒绝原因 key（默认 "{name}.reject_reason"）
    pub approve_key: String,                 // 审批通过 key（默认 "{name}.approved"）
}
```

**仅支持流式模式。** 阻塞模式直接报错，引导使用 `execute_stream()`。

Builder 方法：`BarrierNode::new(name)`, `.timeout()`, `.default_action()`, `.reject_key()`, `.approve_key()`。

**执行流程（三级传递）：**

```
GraphHandle::decide(barrier_id, decision)
  → mpsc::Sender<BarrierDecisionMessage>
  → executor 的 DecisionRegistry 缓存
  → wait_barrier_decision() 取出并 apply_decision()
```

1. BarrierNode 返回 `StreamNodeResult::BarrierPaused`，executor 发射 `BarrierWaiting` 事件
2. executor 调用 `wait_barrier_decision()` — 先查 `DecisionRegistry` 缓存，再 drain channel
3. 用户通过 `GraphHandle::decide()` 或 `GraphHandle::decide_wildcard()` 提交决策
4. executor 接收决策：匹配的立即返回，不匹配的缓存（**level-triggered 语义**）
5. executor 调用 `BarrierNode::apply_decision(decision, state)` 应用决策

**Level-triggered 原则：** 决策提交早于 Barrier 激活 MUST 被保留。
这是 correctness 要求，不是性能优化。

**循环中多次到达：**
- 默认 **Per-Instance**：每次到达生成新 `BarrierId`（node_id + occurrence），必须重新决策
- 可选 `decide_wildcard()`：通配决策匹配指定 node_id 的所有 occurrence

**DecisionRegistry 设计：**
- `HashMap<BarrierId, BarrierDecision>` — plain HashMap，无需 Arc/Mutex
- 唯一消费者是 GraphExecutor 的 spawned task
- 支持精确匹配 + 通配匹配（`decide_wildcard`）

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

### GraphNode Trait

```rust
#[async_trait]
pub trait GraphNode: Send + Sync {
    /// 阻塞执行
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError>;

    /// 流式执行，将内部事件转发到 channel
    /// 默认实现直接调用 execute，返回 StreamNodeResult::Done
    /// AgentNode 覆写以转发 AgentEvent
    /// BarrierNode 覆写以返回 StreamNodeResult::BarrierPaused
    async fn execute_stream(
        &self,
        state: &mut State,
        sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        span_id: SpanId,
    ) -> Result<StreamNodeResult, GraphError>;
}
```

**签名说明：**
- `span_id` — 执行实例 ID，区分同一节点的不同执行（回跳循环）
- 返回 `StreamNodeResult` 而非 `NextStep` — 支持 BarrierPaused 和 Observed 变体
- Barrier 决策不通过此签名传递（走 `GraphHandle::decide` + `DecisionRegistry`）

**StreamNodeResult 变体：**

```rust
pub enum StreamNodeResult {
    Done { next: NextStep, span_id: SpanId },
    BarrierPaused { barrier_id, node_name, span_id, timeout, default_action },
    Observed { error: ObservedError, next: NextStep, span_id: SpanId },
}
```

---

## [S3] Graph 结构

### Edge 语义模型

```rust
pub type EdgeCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;

pub struct Edge {
    pub from: String,
    pub to: String,
    pub condition: Option<EdgeCondition>,    // 业务路由条件（必须满足）
    pub analysis: Option<EdgeAnalysis>,      // 分析用约束（不参与 runtime 决策）
    pub fallback: bool,                      // 兜底边
}
```

**关键分界：**
- `condition` = "走哪条路"（运行时决策）
- `analysis` = "你可能会出事"（静态分析用，如 `analyze_cycles()`）
- `fallback` = "兜底路径"（无匹配时尝试）

```rust
pub struct EdgeAnalysis {
    pub max_visits: Option<usize>,  // 建议的最大访问次数 — 仅用于循环分析诊断
}
```

> **为什么没有 EdgePolicy？** MaxVisits 本质上是循环控制/安全保护，不是路由策略。
> 运行时安全由 `GraphExecutor::max_steps` 统一负责。
> 如果未来出现 timeout/retry/rate limit 需求，会上移到 `ExecutionPolicy` / `GraphPolicy`，而不是塞进 Edge。

### Graph

```rust
pub struct Graph {
    pub(crate) nodes: IndexMap<String, NodeKind>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) start: String,
    pub(crate) end: String,
}
```

**图允许有环。** 循环保护由三层体系提供。

方法：
- `node_names()` / `start_node()` / `end_node()` — 基础查询
- `edges_from(from)` — 获取从指定节点出发的边
- `find_edge(from, to)` — 查找特定边
- `find_fallback_edge(from)` — 查找兜底边（RecoverableError 恢复用）
- `validate()` — 结构验证（节点/边引用有效性，**不检测环**）
- `analyze_cycles()` — 诊断用，找出图中所有环，生成 `CycleAnalysis` 报告

### CycleAnalysis — 环分析诊断

```rust
pub struct CycleAnalysis {
    pub has_cycles: bool,
    pub cycles: Vec<Vec<String>>,           // 所有环
    pub unprotected_cycles: Vec<Vec<String>>, // 无保护的环
    pub total_edges: usize,
    pub protected_edges: usize,
}
```

`analyze_cycles()` 输出结构化报告，标注哪些环有 analysis/policy 保护，哪些无保护。

**不阻止构建，仅用于调试和审查。**

### 验证规则

`validate()` 检查：
1. 起始节点存在
2. 结束节点存在
3. 所有边引用的节点存在

**不检查：**
- 循环（有环图合法）
- 节点可达性（未实现）
- 条件边覆盖完整性（未实现）

---

## [S4] State 设计

### 两层 API 架构

底层保持动态 KV，上层提供强类型访问与 Reducer 合并机制。

```
用户代码
  ↓
StateExt（强类型 getter/setter + Reducer）
  ↓
State = HashMap<String, Value>（底层动态存储）
```

### 底层：动态 State

```rust
pub type State = HashMap<String, serde_json::Value>;
```

**为什么不做强类型 State？**

Graph 的状态天然是开放集合（open world）——Agent、Tool、Barrier、Subgraph 各自读写不同的字段。强类型 State（如 `GraphBuilder::<MyState>::new()`）最终会退化为 `known_fields + dynamic_fields`，收益不高。

### 上层：StateExt

```rust
pub trait StateExt {
    fn get_str(&self, key: &str) -> Option<&str>;
    fn get_bool(&self, key: &str) -> Option<bool>;
    fn get_u64(&self, key: &str) -> Option<u64>;
    fn get_i64(&self, key: &str) -> Option<i64>;
    fn get_f64(&self, key: &str) -> Option<f64>;
    fn get_json<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: DeserializeOwned;
    fn require<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: DeserializeOwned;
    fn set<T>(&mut self, key: impl Into<String>, value: T)
    where
        T: Serialize;
    fn remove(&mut self, key: &str) -> Option<Value>;
    fn contains(&self, key: &str) -> bool;

    // Reducer 合并
    fn reduce(&mut self, key: &str, value: Value, reducer: &StateReducer) -> Result<(), String>;
    fn append_array(&mut self, key: &str, items: Value) -> Result<(), String>;
}
```

消除 `as_str().unwrap()` / `serde_json::from_value()` 样板代码。

### StateError

```rust
pub enum NodeEvent {
    Agent(lellm_agent::AgentEvent),
}
```

### Reducer 机制

```rust
pub type StateReducer = Box<
    dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync,
>;

/// 内置：数组追加（类似 LangGraph 的 operator.add for lists）
pub fn array_reducer() -> StateReducer;
```

### 执行结果

```rust
pub struct GraphResult {
    pub trace_id: TraceId,
    pub state: State,
    pub execution_log: Vec<ExecutionEntry>,
    pub duration: Duration,
}

pub struct ExecutionEntry {
    pub step: usize,
    pub node_name: String,
    pub start_time: Instant,
    pub end_time: Instant,
    pub success: bool,
}
```
用户代码
  ↓
StateExt（强类型 getter/setter + Reducer）
  ↓
State = HashMap<String, Value>（底层动态存储）
```

### 底层：动态 State

```rust
pub type State = HashMap<String, serde_json::Value>;
```

**为什么不做强类型 State？**

Graph 的状态天然是开放集合（open world）——Agent、Tool、Barrier、Subgraph 各自读写不同的字段。强类型 State（如 `GraphBuilder::<MyState>::new()`）最终会退化为 `known_fields + dynamic_fields`，收益不高。

### 上层：StateExt

```rust
pub trait StateExt {
    fn get_str(&self, key: &str) -> Option<&str>;
    fn get_bool(&self, key: &str) -> Option<bool>;
    fn get_u64(&self, key: &str) -> Option<u64>;
    fn get_i64(&self, key: &str) -> Option<i64>;
    fn get_f64(&self, key: &str) -> Option<f64>;
    fn get_json<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: DeserializeOwned;
    fn require<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: DeserializeOwned;
    fn set<T>(&mut self, key: impl Into<String>, value: T)
    where
        T: Serialize;
    fn remove(&mut self, key: &str) -> Option<Value>;
    fn contains(&self, key: &str) -> bool;

    // Reducer 合并
    fn reduce(&mut self, key: &str, value: Value, reducer: &StateReducer) -> Result<(), String>;
    fn append_array(&mut self, key: &str, items: Value) -> Result<(), String>;
}
```

消除 `as_str().unwrap()` / `serde_json::from_value()` 样板代码。

### StateError

```rust
pub enum StateError {
    MissingKey(String),
    Deserialize(String, String),
}
```

### Reducer 机制

```rust
pub type StateReducer = Box<
    dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync,
>;

/// 内置：数组追加（类似 LangGraph 的 operator.add for lists）
pub fn array_reducer() -> StateReducer;
```

### 执行结果

```rust
pub struct GraphResult {
    pub trace_id: TraceId,
    pub state: State,
    pub execution_log: Vec<ExecutionEntry>,
    pub duration: Duration,
}

pub struct ExecutionEntry {
    pub step: usize,
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
| 阻塞 | `execute(Arc<Graph>, state)` | `Result<GraphResult, GraphError>` | 简单流水线、测试（**不支持 BarrierNode**） |
| 流式 | `execute_stream(Arc<Graph>, state)` | `GraphExecution { stream, handle }` | 需要实时事件、BarrierNode |

> **Stream is primary, Blocking is derived.** `execute()` 内部消费 stream 直到结束。

**BarrierNode 限制：** 阻塞模式会提前检查图中是否包含 BarrierNode，有则返回错误引导使用 `execute_stream()`。

### 执行流程（流式模式）

```
GraphExecution { stream, handle } = executor.execute_stream(graph, state)
handle.drop()  →  cancel_tx 不触发取消（executor 持有 rx）

current = start_node
loop {
  if step > max_steps → StepsExceeded 熔断
  if current == end_node → break
  result = node.execute_stream(state, sink, span_id)
  match result {
    Done { next }     → resolve_next(graph, current, state, edge_visits, next)
    BarrierPaused     → DecisionRegistry 缓存 → wait_barrier_decision()
    Observed { error } → 发射 ObservedError 事件，继续执行
    Err(Terminal)     → 发射 GraphError 事件，break
    Err(Recoverable)  → 尝试 fallback 路径
  }
}
// 正常结束 → GraphComplete
```

### resolve_next — 路由解析

处理 `NextStep` 为目标节点名称，统一跳转校验。

| NextStep | 行为 |
|----------|------|
| `Goto(target)` | 查找 `from→target` 边，验证存在 |
| `GoToNext` | `find_next_node()` 按优先级查找下一个节点 |
| `End` | 返回 `TerminalError::InvalidGraph("unexpected End")` |

### find_next_node — 三类边 + 有序路由规则

一个节点的出边分为三类，按固定顺序求值：

| 类别 | API | 语义 | 数量 |
|------|-----|------|------|
| **条件边** | `edge_if(from, to, cond)` | `if/else-if` 规则链，按注册顺序求值 | 0~N 条 |
| **普通边** | `edge(from, to)` | 无条件兜底（非 fallback） | 0~N 条 |
| **Fallback 边** | `edge_fallback(from, to)` | 最后兜底 | 0~N 条 |

**路由规则（first match wins）：**

```
1. 条件边 — 按注册顺序求值，第一条命中即停止（if/else-if 语义）
   ↓ 无命中
2. 普通边 — 无条件非 fallback，取第一条
   ↓ 无普通边
3. Fallback 边 — 无条件 fallback，取第一条
   ↓ 无匹配
4. Unrouted TerminalError（附带所有条件的评估结果）
```

**关键语义：**

- 条件边是**有序规则链**（registration order = priority），不是平级集合
- 多个条件边都匹配时，只有第一条生效
- 普通边与条件边互斥：有条件边命中时，普通边不会走到
- Fallback 边是最后的安全网，无条件匹配
- **不允许** `edge_fallback_if`（带条件的 fallback）——当前 API 无此方法

**警告：** `validate()` 在检测到同一节点有多条条件边时，输出 Warning（非 Error）提醒用户注意顺序。

### 流式执行

`execute_stream()` 返回 `GraphExecution { stream, handle }`。

#### 事件分层

```
GraphEvent（图级）
  ├── GraphStart            — 执行开始（携带 trace_id）
  ├── NodeStart / NodeEnd   — 节点执行边界
  ├── Node                  — 节点内部事件（NodeEvent）
  │     └── Agent(AgentEvent) — Agent 内部事件
  ├── BarrierWaiting        — 等待外部审批（需响应）
  ├── BarrierResolved       — 决策已应用
  ├── ObservedError         — 观测错误（不影响控制流）
  ├── GraphComplete         — 执行完成
  └── GraphError            — 执行出错
```

**Graph 编排 Agent，不暴露 Agent 内部实现。** `NodeEvent` 是中间层，隔离 `GraphEvent` 与节点内部事件。

```rust
pub struct TraceId(Uuid);
pub struct SpanId(Uuid);

pub enum NodeEvent {
    Agent(lellm_agent::AgentEvent),
}

pub enum GraphEvent {
    NodeStart { node_name: String, span_id: SpanId, step: usize },
    NodeEnd { node_name: String, span_id: SpanId, success: bool, duration: Duration },
    Node { span_id: SpanId, node_name: String, event: NodeEvent },
    BarrierWaiting { barrier_id: BarrierId, node_name: String, span_id: SpanId },
    BarrierResolved { barrier_id: BarrierId, decision: BarrierDecision },
    ObservedError { error: ObservedError, node_name: String },
    GraphComplete { result: GraphResult },
    GraphError { error: GraphError, state: State },
}
```

#### 为什么需要 `SpanId`

`SpanId` 的价值不在并发，而在**生命周期追踪**。

同一个节点可能被多次执行：

```
Graph: planner → coder → reviewer → planner (回跳)

事件流：
planner/7f12...  Token
planner/7f12...  ToolCall
coder/a82c...    Token
coder/a82c...    ToolCall
reviewer/b93d... Token
planner/c3e1...  Token    ← 第二次执行 planner，不同的 span_id
planner/c3e1...  Complete
```

`node_name` 只能区分"哪个节点"，`span_id` 区分"哪次运行"。日志天然可聚合。

#### Barrier 决策 API

`BarrierWaiting` 不暴露内部同步原语（`oneshot::Sender`），而是通过 `GraphHandle` 提交决策：

```rust
pub struct GraphHandle {
    decision_tx: mpsc::Sender<BarrierDecisionMessage>,
    cancel_tx: mpsc::Sender<()>,
}

impl GraphHandle {
    /// 提交 Barrier 决策（精确匹配）
    pub async fn decide(
        &self,
        barrier_id: BarrierId,
        decision: BarrierDecision,
    ) -> Result<(), GraphError>;

    /// 提交通配决策 — 匹配指定 node_id 的所有 occurrence
    pub async fn decide_wildcard(
        &self,
        node_id: impl Into<String>,
        decision: BarrierDecision,
    ) -> Result<(), GraphError>;

    /// 强制取消正在执行的 Graph
    pub fn cancel(&self);
}
```

**使用方式：**
```rust
let GraphExecution { mut stream, handle } = executor.execute_stream(graph, state);
while let Some(event) = stream.recv().await {
    match event {
        GraphEvent::BarrierWaiting { barrier_id, node_name, .. } => {
            let decision = ask_user(&node_name).await;
            handle.decide(barrier_id, decision).await?;
        }
        GraphEvent::GraphComplete { result } => { /* ... */ }
        _ => {}
    }
}
```

**设计优势：**
1. **事件可序列化** — `GraphEvent` 不含 `Sender`，可直接转 JSON 推送给 Web UI
2. **支持 Remote UI** — 浏览器收到 `barrier_waiting` 事件，通过 HTTP/WebSocket 调用 `decide()`
3. **隐藏内部同步** — executor 私有 `DecisionRegistry` 管理决策缓存，调用方不受影响
4. **Level-triggered** — 决策顺序 ≠ Barrier 到达顺序时不会丢失（缓存机制）
5. **无同步原语开销** — `DecisionRegistry` 是 plain HashMap，无需 Arc/Mutex

**生命周期契约：**
- 正常结束：`GraphComplete` 恰好一次，然后 channel 关闭
- 异常结束：`GraphError` 恰好一次，然后 channel 关闭
- 终态事件后不再发送任何事件

### 错误处理

**错误二分法：** GraphError 分为 Terminal / Recoverable 两级。

```rust
pub enum GraphError {
    Terminal(TerminalError),       // 终止执行，stream 关闭
    Recoverable(RecoverableError), // 内部重试 / fallback，stream 继续
}
```

**TerminalError：** InvalidGraph, NodeNotFound, MissingEdge, NodeExecutionFailed, StepsExceeded, LoopLimitExceeded, BarrierTimeout, BarrierCancelled, Unrouted, StateError

**RecoverableError：** Retryable（节点重试）, FallbackTriggered（边 fallback 触发）

**Recoverable 恕复路径：** executor 检测到 `Recoverable` 时，尝试寻找 fallback 边跳转。无 fallback 则降级为 `Terminal`。

**可观测性（Warning/Diagnostic）：** 不属于错误体系，通过 `GraphEvent::ObservedError` 事件发送，节点通过 `StreamNodeResult::Observed` 声明。

---

## [S6] Builder API

GraphBuilder 是**图编辑器**（Graph Editor），不是配置 DSL。
核心 API 为 `&mut self` 可变式，所有方法返回 `Result`。

### 主要用法

```rust
let mut g = GraphBuilder::new("workflow");

g.start("init")?;
g.node("init", NodeKind::Task(TaskNode::new("init", |state| { ... })))?;
g.node("agent", NodeKind::Agent(Box::new(AgentNode::new("agent", agent)
    .with_output("agent.output")
    .with_messages("agent.messages"))))?;

// 循环注册节点
for tool in tools {
    g.node(tool.name(), tool_node(tool))?;
}

g.edge("init", "agent");
g.edge_if("agent", "check", |s| s.has_tool_calls())?;
g.edge("agent", "done");
g.end("done")?;

let graph = g.build()?;
```

### 链式 API — PendingEdge

`edge()` / `edge_if()` / `edge_fallback()` 返回 `PendingEdge`，支持链式附加分析约束：

```rust
// 普通边 + 循环分析
g.edge("b", "a").max_visits(5);

// 条件回跳 + 循环分析
g.edge_if("agent", "retry", |s| s.should_retry)?.max_visits(10);

// fallback 边 + 循环分析
g.edge_fallback("agent", "safe").max_visits(3);

// 不加分析（直接丢弃 PendingEdge）
g.edge("agent", "end");
```

### 模块化注册

```rust
let mut g = GraphBuilder::new("workflow");
g.start("init")?;

register_auth_nodes(&mut g)?;
register_mcp_nodes(&mut g)?;
register_review_nodes(&mut g)?;

g.end("done")?;
let graph = g.build()?;
```

### GraphBuilder 方法

| 方法 | 返回 | 说明 |
|------|------|------|
| `new(name)` | `Self` | 创建构建器 |
| `start(node)` | `Result<&mut Self>` | 设置起始节点 |
| `end(node)` | `Result<&mut Self>` | 设置结束节点 |
| `node(name, kind)` | `Result<&mut Self>` | 添加节点（重复名报错）|
| `edge(from, to)` | `PendingEdge` | 添加普通边（无条件非 fallback）|
| `edge_if(from, to, cond)` | `Result<PendingEdge>` | 添加条件边 |
| `edge_fallback(from, to)` | `PendingEdge` | 添加 fallback 边 |
| `build()` | `Result<Graph>` | 构建并验证 |

**PendingEdge 链式方法：**

| 方法 | 返回 | 说明 |
|------|------|------|
| `.max_visits(n)` | `&mut GraphBuilder` | 附加循环分析约束（仅诊断）|
| `build()` | `self -> Result<Graph, BuildErrors>` | 构建并验证（收集所有错误）|

**设计原则：**
- 所有验证返回 `Result`，不允许 panic
- `build()` 消费 Builder，执行最终验证
- `BuildError` 仅验证结构完整性，不检测循环、业务逻辑漏洞

---

## [S7] 错误类型

### BuildError — 构建时结构校验

```rust
pub enum BuildError {
    DuplicateNode { id: String },
    MissingNode { from: String, to: String },
    MissingEntryPoint,
    MissingExitPoint,
    InvalidEdgeDefinition { from: String, to: String, reason: String },
}
```

**不管：** 循环、业务逻辑漏洞、运行时 unreachable。

### GraphError — 运行时二分法

```rust
pub enum GraphError {
    Terminal(TerminalError),       // 终止执行
    Recoverable(RecoverableError), // 可恢复，stream 继续
}
```

### TerminalError

```rust
pub enum TerminalError {
    InvalidGraph(String),
    NodeNotFound(String),
    MissingEdge { from: String, to: String },
    NodeExecutionFailed { node: String, source: Box<dyn Error + Send + Sync> },
    StepsExceeded { limit: usize },
    LoopLimitExceeded { limit: usize },
    BarrierTimeout { node: String, timeout: Duration },
    BarrierCancelled { node: String },
    Unrouted { node: String, attempted_conditions: Vec<ConditionEval> },
    StateError(String),
}
```

### RecoverableError

```rust
pub enum RecoverableError {
    Retryable { node: String, attempt: usize, max_attempts: usize, reason: String },
    FallbackTriggered { from: String, to: String, reason: String },
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
│   └── graph_test.rs             # 集成测试（41 个）
└── src/
    ├── lib.rs                    # 公开 API
    ├── error.rs                  # BuildError, GraphError (二分法), ObservedError
    ├── state.rs                  # State, StateExt, StateReducer, GraphResult, TraceId, SpanId
    ├── statekey.rs               # StateKey<T> 编译期类型安全
    ├── node.rs                   # GraphNode trait, NextStep, NodeKind, StreamNodeResult
    │                              # TaskNode, ConditionNode
    ├── llm_node.rs               # AgentNode (with_output/with_messages), LLMNode (with_tools)
    ├── tool_node.rs              # ToolNode (追加模式)
    ├── barrier_node.rs           # BarrierNode, BarrierDecision, BarrierDefaultAction
    ├── event.rs                  # GraphEvent, GraphStream, GraphHandle, SpanId, BarrierId
    ├── graph.rs                  # Graph (Clone + name), Edge (PendingEdge), EdgeAnalysis, GraphBuilder
    └── executor.rs               # GraphExecutor（阻塞 + 流式）, DecisionRegistry
```

---

## [S9] 测试覆盖

| 场景 | 测试 | 状态 |
|------|------|------|
| 线性流水线 | `test_linear_pipeline` | ✅ |
| 条件分支 | `test_condition_branching` | ✅ |
| 节点错误 | `test_task_node_error` | ✅ |
| Goto 缺失边 | `test_goto_missing_edge_error` | ✅ |
| Goto 边 + analysis | `test_goto_edge_with_analysis` | ✅ |
| 有环图构建 | `test_cyclic_graph_allowed` | ✅ |
| 有环图熔断 | `test_cyclic_graph_steps_exceeded` | ✅ |
| 有环图 + edge_if 退出 | `test_cyclic_graph_with_edge_if_exit` | ✅ |
| ConditionNode 回跳 | `test_condition_node_back_jump` | ✅ |
| Edge analysis 不参与 runtime | `test_edge_analysis_no_runtime_interference` | ✅ |
| Barrier 阻塞模式报错 | `test_barrier_blocked_mode_error` | ✅ |
| Barrier Approve | `test_barrier_approve` | ✅ |
| Barrier Reject + 回跳 | `test_barrier_reject_with_back_jump` | ✅ |
| Barrier Modify | `test_barrier_modify` | ✅ |
| Barrier 超时 | `test_barrier_timeout` | ✅ |
| Barrier Reroute | `test_barrier_reroute` | ✅ |
| 双重 Barrier 顺序执行 | `test_double_barrier_sequential` | ✅ |
| 执行日志 | `test_execution_log` | ✅ |
| 缺失节点 | `test_missing_node/start/end` | ✅ |
| Stream SpanId | `test_stream_has_span_id` | ✅ |
| TraceId 唯一性 | `test_trace_id_uniqueness` | ✅ |
| TraceId 流式生命周期 | `test_trace_id_full_lifecycle` | ✅ |
| TraceId 阻塞模式 | `test_trace_id_blocking_mode` | ✅ |
| StateExt getter/setter | `test_state_ext_*` (6 个) | ✅ |
| StateKey 读写 | `test_statekey_basic_read_write` | ✅ |
| StateKey 共存 | `test_statekey_coexist_with_stateext` | ✅ |
| StateKey 缺失/类型不匹配 | `test_statekey_missing_key/type_mismatch` | ✅ |
| StateKey Graph 执行 | `test_statekey_in_graph_execution` | ✅ |

---

## [S10] 待实现

| 优先级 | 功能 | 说明 | 状态 |
|--------|------|------|------|
| P0 | 移除 EdgePolicy | 保留 EdgeAnalysis，Runtime 安全由 max_steps 负责 | ✅ |
| P0 | NodeKind 移除 Loop | 循环通过回边表达 | ✅ |
| P0 | AgentNode 显式写入 | 默认不写 State，用户显式绑定 output/messages | ✅ |
| P0 | GraphError 移除 Observed | 可观测性移到事件系统 | ✅ |
| P0 | Builder API 简化 | 方法返回 &mut Self，验证推迟到 build() | ✅ |
| P0 | ToolNode 追加模式 | 只追加 ToolResult，不重写整个 messages | ✅ |
| P0 | ConditionNode otherwise_target | 统一到 edge_fallback | ✅ |
| P1 | `StateKey<T>` | 编译期 key 常量，消除字符串魔法值 | ✅ |
| P1 | Graph Clone + name | 支持测试复用和热更新 | ✅ |
| P1 | TraceId 落地 | 关联 Graph Execution 全链路 | ✅ |
| P1 | LLMNode tools | 支持手动 ReAct 循环 | ✅ |
| P2 | SpanId 持久化 | 关联到 ExecutionEntry，等 ParallelNode | |
| P2 | ReducerRegistry | 并行分支的 State 合并策略 | |
| P2 | ExecutionTrace 扩展 | 元数据（iterations/tool_calls）进入 Trace 而非 State | |
| P3 | ParallelNode | 并行子图，SpanId 真正发挥价值 | |
| P3 | Loop DSL | Builder 层循环语法糖，展开为回边 + 条件节点 | |

---

## [S11] 版本路线图

| 版本 | 范围 | 状态 |
|------|------|------|
| **v0.2** | Graph/Node/Edge + 有环图 + BarrierNode + 流式执行 + 错误二分法 | ✅ |
| **v0.2.1** | Grill 重构：删除 LoopNode/EdgePolicy/Observed/EventLevel，AgentNode 显式写入，Builder 简化，Graph Clone，TraceId，StateKey，LLMNode tools | ✅ |
| **v0.3** | ParallelNode + Checkpoint + Resume + ReducerRegistry | 规划中 |
| **v0.4** | Multi-Agent Orchestration + Durable Execution | 规划中 |

> **注意：** 原始路线图的 v0.3"StateGraph（任意环）"已被 v0.2 路线 B 覆盖——有环图已是 v0.2 的核心特性。

---

## [S12] 与 v0.1 集成

- `AgentNode` 直接持有 `ToolUseLoop`，复用完整的 ReAct 循环
- `LLMNode` 持有 `ResolvedModel`，单次调用 LLM
- `ToolNode` 直接持有 `ToolExecutor`
- 复用 `LlmError`, `Message`, `ContentBlock` 等核心类型
- `LoopDetector`/`SignalVoter` 在 `v02-preview` feature gate 后，未默认开启

---

## [S13] Checkpoint + Resume + Fork（v0.3 预留）

### Checkpoint 内容

```rust
struct Checkpoint {
    checkpoint_id: CheckpointId,
    trace_id: TraceId,
    span_id: SpanId,
    step: u32,
    state: State,          // 完整快照
    current_node: NodeId,
    metadata: CheckpointMetadata,
}
```

- 保存完整 State（非 Delta）
- `graph_hash` 确保图结构可校验

### Resume 语义

- **新 trace_id，关联原 trace_id**（`original_trace_id` + `resumed_from` + `resume_count`）
- **图可以变**，校验 `graph_hash`，变了就 warn
- **Step 计数器从 checkpoint 继续累加**，不重置（防止无限 resume 绕过 max_steps）

### Fork

- 深拷贝 State，共享 Graph
- 可并发执行（独立 executor + stream + handle）
- Fork trace 关联 parent trace

### 存储层抽象

```rust
trait CheckpointStore {
    async fn save(&self, cp: &Checkpoint) -> Result<CheckpointId>;
    async fn load(&self, id: CheckpointId) -> Result<Checkpoint>;
    async fn list(&self, trace_id: TraceId) -> Result<Vec<Checkpoint>>;
    async fn delete(&self, id: CheckpointId) -> Result<()>;
}
```

v0.3 提供 `MemoryStore` + `FileStore`。Redis 放 v0.4+。

---

## [S14] Grill 结论 — 架构决策总览

| 议题 | 结论 | 状态 |
|------|------|------|
| LoopNode 定位 | 已删除。循环通过回边表达，Runtime 不需要专门的 Loop 节点 | ✅ |
| EdgePolicy | 已删除。保留 EdgeAnalysis + max_steps | ✅ |
| AgentNode 写入 | 改为显式声明（with_output/with_messages），元数据进 ExecutionTrace | ✅ |
| SubGraph | 已删除。SubGraph 仅被 LoopNode 使用，一并清理 | ✅ |
| GraphError Observed | 移到事件系统，GraphError 只保留 Terminal + Recoverable | ✅ |
| BarrierNode 仅流式 | 正确设计决策。Barrier = suspension point，v0.3 引入 Checkpoint/Resume | ✅ 确认 |
| StateKey<T> | 编译期类型安全，消除字符串魔法值 | ✅ |
| TraceId | 贯穿执行生命周期，GraphResult + GraphEvent 全链路 | ✅ |
| Graph Clone | 所有节点 + Graph 实现 Clone，支持测试复用和热更新 | ✅ |
| Builder API | 返回 &mut Self，验证推迟到 build()，PendingEdge 链式 API | ✅ |
| ToolNode | 只追加 ToolResult，不重写整个 messages | ✅ |
| ConditionNode otherwise_target | 统一到 edge_fallback | ✅ |
| EventLevel | 删除。消费者按变体类型过滤 | ✅ |
| BarrierInnerEvent | 删除。NodeEvent 只保留 Agent | ✅ |
| LLMNode tools | 支持手动 ReAct 循环 | ✅ |

> **系统级定性：** LeLLM Graph 的 Runtime Core 只理解一种循环模型（回边）、
> 一种安全机制（max_steps）、一种错误分类（Terminal / Recoverable）。
> 高级语法（Loop DSL）、策略、可观测性都应该在各自的层次处理。
