# LangGraph 与 LeLLM Graph 设计对比

> 基于 LangGraph 官方 Quickstart Tutorial，对比分析两种编排架构的设计取舍。
> 更新于 2026-06-25，与 v0.4 代码深度对齐。

## 核心对撞：微观自由度 vs 宏观确定性

LangGraph（以 Python/JS 为代表的动态语言图生态）与 LeLLM（以 Rust 为代表的系统级类型图生态）在架构哲学上存在根本性分水岭。这个差异不仅体现在控制流和状态管理的底层实现上，更决定了框架的抽象颗粒度。

---

## 一、功能对照表

| 维度 | LangGraph (Python/JS) | LeLLM (Rust) |
|------|----------------------|--------------|
| **状态管理** | `TypedDict` / `zod` schema + `operator.add` reducer | 双层：`State`（`HashMap<String, Value>`，向后兼容）+ `WorkflowState` trait（编译期类型安全，Effect 驱动，`Self` 为类型化状态）；`StateKey<T>` 编译期类型安全的键；`BranchState` 一层 Overlay 模型 |
| **状态变更** | 节点直接修改 State dict | `StateEffect` 领域事件 + `NodeContext::emit_effect()` → Executor 批量 `apply_batch()`；节点不直接写 State |
| **工具定义** | `@tool` decorator / `tool()` + zod schema | `#[derive(Tool)]` macro + `ToolRegistration` + `ToolCatalog` 动态发现 + `CompositeCatalog` 多源合并 |
| **Agent 循环** | 手动构建 `llm_node → tool_node → condition → back` | 双模式：(1) `ToolUseLoop` 内置 ReAct 循环（黑盒封装）；(2) `use_react_graph(true)` 内部构建 `Graph<AgentState>` ReAct 有环图（LLMNode → BudgetCondition → CompactorNode → ToolNode → PostLLMGuard） |
| **条件路由** | `should_continue()` 函数返回 node name / `END` | `ConditionNode` 声明式分支 + `edge_if()` 条件边（first-match-wins）；三元优先级：条件边 > 普通边 > Fallback 边 |
| **图构建** | `StateGraph.add_node().add_edge().compile()` (fail-fast) | `GraphBuilder.node().edge().build()` (多错误收集，返回 `BuildErrors`)；`PendingEdge` 链式调用支持 `.max_visits()` |
| **构建校验** | fail-fast（遇错即停） | 多错误收集 + `GraphDiagnostics` 诊断（非致命变体）+ 重复节点检测 + Fallback 自环检测 |
| **执行** | `agent.invoke({messages: [...]})` | `GraphExecutor::execute()` (阻塞，消费 Stream) / `execute_stream()` (流式，返回 `GraphExecution{stream, handle}`) / `run_inline()` (内联，无 RuntimeEvent) |
| **循环支持** | 天然支持（边可回环） | 允许有环图 + `max_steps` 运行时熔断 + `CycleAnalysis` 静态诊断 + `max_visits` 边级保护 |
| **错误分类** | Exception 机制 | 二分法：`TerminalError`（不可恢复，终止执行）/ `ObservedError`（可观测，不影响控制流）；Fallback 通过 `edge_fallback()` 构建期边配置 + `StreamNodeResult::Fallback` 控制流（非错误变体）|
| **Human-in-the-loop** | `InterruptBefore` + `update_state` / `restart` | `BarrierNode` + `GraphHandle::decide()` — 支持 Approve/Reject/Modify/Reroute + 超时 + wildcard 决策 + CancellationToken |
| **流式输出** | `astream()` 返回检查点变更事件 | `GraphStream` (`mpsc::Receiver<GraphEvent>`) — 全链路事件（GraphStart/End, NodeStart/End, FlowEvent, BarrierWaiting/Resolved, CheckpointSaved）+ TraceId/SpanId + `RuntimeEvent` 可观测层 |
| **持久化** | Checkpointing（SQLite/PostgreSQL/Shallow） | `CheckpointStore` trait + `Checkpoint<S>` 泛型 + `CheckpointPolicy` (EveryNode/BarrierOnly/Manual) + `GraphExecutor::with_checkpoint()` — **已实现** |
| **并行节点** | `Send()` + 并行执行 | `ParallelNode<S, M>` + `MergeStrategy<S>` trait + `ParallelNodeBuilder` — 每个分支独立 State clone，`MergeStrategy` 合并；**已实现**（当前顺序执行，API 完备）|
| **子图/嵌套** | 支持（`StateGraph` 可嵌套） | `Graph::run_inline()` 内联执行子图 + `NodeContext` 传递控制信号 — **部分实现** |
| **节点扩展** | 自定义 node 函数 | `NodeKind` 枚举（Task/Condition/Barrier/Parallel/External）+ `FlowNode<S>` trait + `AgentFlowNode` 桥接 Agent → Graph |
| **Hooks** | 无原生 hook 系统 | `AgentHook` trait — 12 个生命周期回调（on_node_start/end/failed, on_state_changed, on_barrier_waiting/resolved, on_route_decision, on_graph_start/complete/error, on_observed_error）|

---

## 二、核心架构差异深度分析

### 1. Agent 循环：LangGraph 的"平铺" vs LeLLM 的"三层封装"

#### LangGraph — 拓扑层展开

在 LangGraph 里，工具循环（Tool Loop）是在图的拓扑层（Topological Layer）直接展开的。大模型吐出 Tool Call，图导航到 `tool_node`，执行完再路由回 `llm_node`。

```
START → llm_node → tool_node → (should_continue) → llm_node → ... → END
```

**代价：** 图的规模急剧膨胀。一个简单的 ReAct 智能体，在图里就需要 3 个节点和 2~3 条边。如果想在图里加入更宏观的流程（例如：先规划，再执行 ReAct，最后审查），整张图的 DAG 连接会变成密密麻麻的蜘蛛网，极其难以维护。

#### LeLLM — 三层封装

LeLLM 提供三种粒度，从微观到宏观：

**第一层：`ToolUseLoop`（黑盒 ReAct）**

最内层，将 LLM ↔ Tools 的流式交互封装为独立循环，含 Retry/Fallback/ContextCompactor。

```
init → AgentNode(内部: ToolUseLoop) → summary → END
              ┌─────────────────────┐
              │ LLM → Tools → LLM   │  ← 自动循环
              │ (含 Retry/Fallback) │
              └─────────────────────┘
```

**第二层：`AgentFlowNode::use_react_graph(true)`（内部有环图）**

将 ReAct 循环展开为 `Graph<AgentState>` 内部的有环图，使用强类型 `AgentState`（零序列化）：

```
budget_check → llm → post_llm_check → (has_tool_calls?) → tool → budget_check
    ↑                                                    │
    └──── compactor ← (budget_exceeded)                   └──── (no_calls) → end
```

涉及 6 个节点：`BudgetCondition`（预算检查）、`LLMNode`（单次调用）、`PostLLMGuard`（后置守卫）、`ToolNode`（工具执行）、`CompactorNode`（上下文压缩）、`TaskNode::end`（终端）。

**第三层：宏观 Graph 编排（多 Agent 网络）**

将 `AgentFlowNode` 作为 `NodeKind::External` 嵌入宏观 Graph：

```rust
graph.node("agent_react", NodeKind::External(Arc::new(agent)));
```

**优势：** LeLLM 天然具备构建 **Multi-Agent 层次化网络（Hierarchical Agent Networks）** 的顶级底座能力。图只需要关心这个 Node 的输入 State 和输出 State（通过 `StateEffect` 声明式写入）。

---

### 2. 状态管理：Effect 驱动 + Overlay 模型

#### LangGraph — 隐式 Reducer

Python 的动态特性允许用户在全局 State 的某个 Key 上绑定一个隐式的 reducer 闭包：

```python
class MessagesState(TypedDict):
    messages: Annotated[list[AnyMessage], operator.add]
```

当多个节点返回数据时，LangGraph 自动在后台进行隐式合并。

**痛点：** 缺乏静态可预测性。在复杂的并发场景或 Parallel Node 汇聚时，这种隐式的、跨越多个节点的 State 合并极易触发黑盒 Bug。

#### LeLLM — Effect 驱动 + BranchState Overlay

v0.4 引入了完整的 Effect 驱动架构：

```rust
// 节点通过 Effect 声明状态变更意图（不直接写 State）
// StateEffect 只有 Put / Delete 两个变体
ctx.emit_effect(StateEffect::Put("messages", json!(messages)));
ctx.emit_effect(StateEffect::Put("steps", json!("approved")));
ctx.emit_effect(StateEffect::Delete("temp_key"));

// 类型化 State 使用自定义 Effect enum（零序列化）
// 例如 AgentEffect::AppendMessage, AgentEffect::IncrementIteration 等
ctx.emit_effect(AgentEffect::AppendMessage(msg));

// Executor 统一消费 Effects → apply 到 typed state（零序列化）
let effects = ctx.consume_effects();
ctx.state_mut().apply_batch(effects);
```

**BranchState — 一层 Overlay 模型：**

```
BranchState
├── base: Arc<State>         ← 不可变快照（全量 Checkpoint）
├── local: HashMap           ← 本层写入缓存（增量）
└── changes: Vec<Record>     ← 变更日志（审计 + 增量 Checkpoint）

读取 = O(1)：最多查两层（local + base）
写入 = 自动记 ChangeRecord
fork = O(n)：materialize(base + local) → 新 base（n = key 数量）
```

**WorkflowState trait — 编译期类型安全：**

```rust
pub trait WorkflowState: Clone + Send + Sync + Serialize + DeserializeOwned {
    type Effect: Effect;
    fn apply(&mut self, effect: Self::Effect);
    fn apply_batch(&mut self, effects: impl IntoIterator<Item = Self::Effect>);
    fn apply_branch_change(&mut self, change: &ChangeRecord); // backward compat, default no-op
    fn initial() -> Self where Self: Default;
}

// AgentState 直接 impl，零序列化
impl WorkflowState for AgentState {
    type Effect = AgentEffect;  // AppendMessage, IncrementIteration, AddOutputTokens, ...
    fn apply(&mut self, effect: AgentEffect) { ... }
}
```

**MergeStrategy — 并行合并职责剥离：**

并行合并规则由 Graph 层的 `MergeStrategy<S>` 决定，而非 State 内建属性。`StateMerge`（默认，逐 key 合并，后续分支覆盖同 key）、`LastWriteWins`（最后一个分支整体获胜）、自定义合并策略。

**优势：**
- 显式高于隐式 — 哪个节点、何时、以什么规则修改 State，完全可追踪
- `StateDelta` 记录修改来源（Node/Hook/Reducer/ResumeReplay）
- `StateKey<T>` 编译期类型安全 + 内置 Reducer（Append/Sum/Replace/MergeObject/Max/Min/Error）
- 绝对不会发生数据竞争（Data Race）

---

### 3. 控制流：NextAction + ExecutionSignal 正交分离

#### LangGraph — 节点返回值决定路由

节点通过返回值决定下一步：

```python
def my_node(state):
    result = do_something(state)
    if result.needs_review:
        return {"next": "review"}
    return {"next": "done"}
```

路由逻辑和执行逻辑耦合在同一个函数中。

#### LeLLM — 正交分离

LeLLM 将控制流拆分为三个正交维度：

```rust
// 1. NextAction — 拓扑路由（Next/Goto/End）
pub enum NextAction {
    Next,      // 按拓扑顺序走下一步
    Goto(String), // 跳转到指定节点
    End,       // 结束执行
}

// 2. ExecutionSignal — 运行时信号（Barrier 挂起等）
pub enum ExecutionSignal {
    Pause { barrier_id: BarrierId, timeout: Option<Duration> },
}

// 3. NodeMetadata — 元数据（Token 成本、副作用标记）
pub struct NodeMetadata {
    pub token_cost: f64,
    pub has_side_effects: bool,
}

// 节点通过 NodeContext 写入控制信号
ctx.goto("review");        // → NextAction::Goto
ctx.end();                 // → NextAction::End
ctx.pause(barrier_id);     // → ExecutionSignal::Pause

// Executor 统一读取
let (next_action, signal) = ctx.take_control();
```

**边解析的三元优先级：**

```rust
// Graph::resolve_next() 的解析顺序：
// 1. 条件边（edge_if）— first-match-wins
// 2. 普通边（edge）— 返回第一条
// 3. Fallback 边（edge_fallback）— 错误恢复路径
```

**优势：** 路由意图、运行时信号、元数据三者完全解耦。ConditionNode 内部调用 `ctx.goto(target)` 设置 `NextAction::Goto`，Executor 的 `resolve_next()` 只处理 `NextAction::Next`。Barrier 通过独立的 `ExecutionSignal::Pause` 挂起执行，不污染路由逻辑。

---

### 4. 循环支持：自由拓扑回环 vs 有环图 + 多层防护

#### LangGraph — 自由拓扑回环

LangGraph 的底层引擎是一个纯粹的 Stateful State Machine（带状态机路由）。通过条件路由函数 `should_continue` 返回字符串形式的节点名，控制流可以任意回溯到图的任何角落。

```python
def should_continue(state: MessagesState) -> Literal["tool_node", END]:
    if state["messages"][-1].tool_calls:
        return "tool_node"  // 回溯到 tool_node
    return END
```

**代价：** 图的拓扑校验（Validation）基本失效。在运行前，你无法通过静态算法发现这张图是否会产生死循环、是否有孤立节点（Dead End）、或者状态机是否会跳转到一个不存在的节点名。所有的错误都只能推迟到运行时通过崩溃来暴露。

#### LeLLM — 多层防护

LeLLM 允许图中存在环，通过四层防护确保安全：

```rust
// 第 1 层：max_steps 运行时熔断
let (result, state) = GraphExecutor::new(50)  // 最多 50 步
    .execute(graph, state)?;
// 超限返回 TerminalError::StepsExceeded

// 第 2 层：max_visits 边级保护
.edge("retry", "agent").max_visits(5)  // 该边最多访问 5 次

// 第 3 层：CycleAnalysis 静态诊断
let analysis = graph.analyze_cycles();
println!("{}", analysis.report());
// "Cycle 1: agent → retry → agent — ⚠️ UNPROTECTED"
// "Cycle 2: a → b → a — ✅ Protected by edge-level analysis"

// 第 4 层：GraphDiagnostics 综合诊断
let diag = graph.analyze();
// 检测：环、不可达节点、end 节点出边、Fallback 在环中的位置
```

**优势：** 既保留了 LangGraph 式的灵活回环能力，又通过多层防护兜底防止无限循环。

---

### 5. 错误处理：Exception vs 二分法 + Fallback 控制流

#### LangGraph — Exception

LangGraph 依赖 Python 的 exception 机制。任何未捕获的异常都会中断图执行，需要外部 try/catch 处理。

#### LeLLM — 二分法 + Fallback 控制流

LeLLM 将错误分为两个正交的类别，Fallback 通过控制流而非错误变体实现：

```rust
// GraphError — 只有 Terminal 变体
// Fallback 通过 StreamNodeResult::Fallback 控制流实现（非错误变体）
pub enum GraphError {
    Terminal(TerminalError),   // 不可恢复，终止执行
}

// TerminalError — 变体丰富
pub enum TerminalError {
    InvalidGraph(String),
    NodeNotFound(String),
    MissingEdge { from, to },
    NodeExecutionFailed { node, source },
    StepsExceeded { limit },
    LoopLimitExceeded { limit },
    Unrouted { node, attempted_conditions },
    StateError(String),
    BarrierTimeout { node, timeout },
    BarrierCancelled { node },
}

// ObservedError — 不影响控制流，仅发射 GraphEvent::ObservedError
pub enum ObservedError {
    Warning { node, message },
    Degraded { node, message },
    PartialFailure { node, succeeded, failed, message },
}
```

配合 `edge_fallback()` 实现降级路由：

```rust
let graph = GraphBuilder::new("resilient")
    .edge_fallback("agent", "degraded_mode")  // agent 失败 → 降级模式
    .build()?;
```

---

### 6. Human-in-the-loop：中断恢复 vs Barrier 决策

#### LangGraph

通过 `InterruptBefore` 在指定节点前中断，然后调用 `update_state()` 修改状态或 `restart()` 继续执行。机制较为底层，需要手动管理中断点。

#### LeLLM

提供 `BarrierNode` 作为专用的审批节点，配合 `GraphHandle::decide()` 提交结构化决策：

```rust
// 定义 Barrier 节点
// default_action 接受 BarrierDefaultAction（Reject/Approve/Skip）
let barrier = BarrierNode::new("approval")
    .timeout(Duration::from_secs(300))
    .default_action(BarrierDefaultAction::Reject)
    .reject_key("rejected")
    .approve_key("approved");

// 图中使用
.node("approval", NodeKind::Barrier(Box::new(barrier)))

// 执行时通过 Handle 提交决策
// BarrierDecision 包含 Approve/Reject/Modify/Reroute
handle.decide(barrier_id, BarrierDecision::Approve)?;
handle.decide(barrier_id, BarrierDecision::Modify { key: "result", value: ... })?;
handle.decide(barrier_id, BarrierDecision::Reroute { target: "review" })?;
// 支持 wildcard 决策 — 对指定 node_id 的 Barrier 统一处理
handle.decide_wildcard("approval", BarrierDecision::Approve)?;

// 取消执行
handle.cancel();
```

决策机制详解：
- **DecisionRegistry** — level-triggered，提前提交的决策会被缓存（`pending` HashMap + `wildcards`）
- **BarrierId** — 由 `node_id` + `occurrence` 组成，支持同一 Barrier 多次触发
- **超时处理** — 使用 `default_action` 自动拒绝或批准

---

### 7. 持久化 / Checkpointing：已实现

#### LangGraph

内置 Checkpointing，支持 SQLite/PostgreSQL/Shallow 存储后端。

#### LeLLM

v0.4 实现了完整的 Checkpoint 架构：

```rust
// Checkpoint — 物化快照 + 执行游标
pub struct Checkpoint<S = State> {
    pub checkpoint_id: CheckpointId,
    pub current_node: NodeId,      // 下一个要执行的节点
    pub state: S,                  // 物化状态快照
    pub created_at: SystemTime,
}

// CheckpointStore trait — 存储后端 SPI
#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn save_with_trace(&self, trace_id, checkpoint) -> Result<()>;
    async fn load(&self, id) -> Result<Option<Checkpoint>>;
    async fn load_latest(&self, trace_id) -> Result<Option<Checkpoint>>;
    async fn list(&self, trace_id) -> Result<Vec<CheckpointId>>;
    async fn delete(&self, id) -> Result<bool>;
    async fn prune(&self, trace_id, keep) -> Result<usize>;
}

// CheckpointPolicy — 控制何时保存
pub enum CheckpointPolicy {
    EveryNode,    // 每次节点执行后保存（默认）
    BarrierOnly,  // 仅在 Barrier 决策后保存
    Manual,       // 手动控制
}

// 使用
let executor = GraphExecutor::with_checkpoint(
    50,
    Arc::new(MyCheckpointStore),
    CheckpointPolicy::BarrierOnly,
    &graph,
);
```

设计原则：
- **Checkpoint 唯一职责是恢复（Restore）** — 不含 parent_trace_id（存储层组织关联）、不含 effect_log（审计走 ExecutionTrace）、不含 snapshot（增量快照是存储层优化）
- **Trace 关联由存储层组织** — 如同一目录下的文件
- **泛型 `<S>`** — 支持类型化 State 的序列化

---

### 8. 并行执行：已实现

#### LangGraph

通过 `Send()` 实现节点级并行。

#### LeLLM

`ParallelNode` 已实现，每个分支独立 State clone，`MergeStrategy` 合并：

```rust
// 构建 ParallelNode
// ParallelNodeBuilder 默认使用 StateMerge（逐 key 合并）
let parallel = ParallelNode::builder()
    .branch("search_web", search_node)
    .branch("search_docs", docs_node)
    .branch("search_code", code_node)
    .error_strategy(ParallelErrorStrategy::FailFast)
    .build();

// 如需切换合并策略，使用 merge_strategy() 返回新类型构建器
let parallel = ParallelNode::builder()
    .branch("search_web", search_node)
    .merge_strategy::<LastWriteWins>()  // 最后一个分支获胜
    .build();

// 图中使用
.node("research", NodeKind::Parallel(parallel))
```

执行流程：
1. 克隆 base State 给每个分支
2. 每个分支获得独立的 `BranchState`（fork 自父节点）
3. 分支执行 → 消费 Effects → apply 到分支 State
4. `MergeStrategy::merge(branches)` 合并所有分支 State
5. 替换父 State 为合并结果

当前为顺序执行（serial fallback），API 层已完备，可升级为 `tokio::join!` 并行。

---

### 9. 内联执行：子图调用的基础

LeLLM 提供 `Graph::run_inline()` 方法，支持在不产生 RuntimeEvent 和 Checkpoint 的情况下内联执行子图：

```rust
// 内联执行 — 不产生 RuntimeEvent，不 Checkpoint
graph.run_inline(ctx, max_steps).await?;
// Effects 直接 apply 到父 NodeContext 的 typed state
// 控制信号通过 NextAction 传递
```

这是实现 `NodeKind::Graph(sub_graph)` 子图嵌套的基础设施。

---

## 三、代码量对比

以 LangGraph Quickstart Tutorial 的计算器示例为例：

| 指标 | LangGraph (Python) | LeLLM (Rust) |
|------|-------------------|--------------|
| 工具定义 | ~45 行 | ~30 行（含 derive 宏） |
| Agent Loop | ~40 行（3 节点 + 条件路由） | ~0 行（`ToolUseLoop` 内置） |
| Graph 构建 | ~10 行 | ~20 行（含 init/summary 节点） |
| 总计 | ~95 行 | ~50 行核心逻辑 |

---

## 四、LeLLM 的工程优越性总结

通过对比可以清晰地看出，LeLLM 并不是在复刻一个 Rust 版的 LangGraph，而是在纠正 LangGraph 在大型生产环境中的工程缺陷：

### 1. 宏观极简，微观极速

将 `ToolUseLoop` 坍缩到节点内部，避免了图拓扑污染。结合 Rust 的原生异步线程池（Tokio），局部并发和流式处理的吞吐量高出动态语言几个数量级。

### 2. Effect 驱动的状态变更

节点通过 `emit_effect()` 声明变更意图，Executor 统一 `apply_batch()`。状态变更完全可追踪（`StateDelta` 记录来源），`BranchState` 的 Overlay 模型提供增量审计（fork = O(n)，n 为 key 数量）。

### 3. 编译期类型安全

`WorkflowState` trait（`Clone + Send + Sync + Serialize + DeserializeOwned`）+ `StateKey<T>` + `MergeStrategy<S>` 构成编译期类型安全的三角。`Graph<AgentState>` 的节点直接读写强类型 struct，零序列化。

### 4. 正交的控制流设计

`NextAction`（拓扑路由）、`ExecutionSignal`（运行时信号）、`NodeMetadata`（元数据）三者解耦。Barrier 挂起不污染路由逻辑，Condition 跳转优先于边解析。

### 5. Stream-First 设计

`execute()` 内部消费 `execute_stream()` 的流 — 流式是首要的执行模式，阻塞模式是派生。全链路 `GraphEvent` 配合 `TraceId`/`SpanId` 贯穿；独立 `RuntimeEvent` enum（`ExecutionStarted`/`NodeStarted`/`NodeCompleted`/`NodeFailed`/`BarrierWaiting`/`BarrierResolved`/`ExecutionCompleted`）通过 `emit_runtime()` 发射，提供可观测性钩子（`AgentHook` trait），开箱即用的可观测性。

### 6. 构建期校验 — 多错误收集

`build()` 一次性收集所有错误后统一报告（而非遇到第一个错误就停止），返回 `Result<Graph, BuildErrors>`：

```rust
// 多错误收集 — 所有问题一次性暴露
match builder.build() {
    Ok(graph) => { /* 使用 graph */ }
    Err(errors) => {
        // 可能包含 MissingNode × 3, DuplicateNode × 1
        for e in errors.iter() {
            eprintln!("{}", e);
        }
    }
}
```

`GraphDiagnostics` 提供非致命诊断（环检测、不可达节点、end 节点出边）。致命错误（`MissingNode`, `MissingEntryPoint` 等）才导致 `build()` 失败。

LangGraph 的 `compile()` 是 fail-fast（遇到第一个错误就抛异常），开发者需要多次修正、多次编译。LeLLM 的多错误收集减少了 edit-compile 循环。

### 7. Human-in-the-loop 一等公民

`BarrierNode` 提供结构化的审批决策（Approve/Reject/Modify/Reroute），配合 `DecisionRegistry` 的 level-triggered 机制和 wildcard 决策。而非 LangGraph 式的底层中断+手动恢复。

### 8. Checkpointing 已落地

`CheckpointStore` trait + `Checkpoint<S>` 泛型 + `CheckpointPolicy` 策略，完整实现持久化。设计哲学：Checkpoint 的唯一职责是恢复，给我一个 Checkpoint 文件就能从 `current_node` 继续执行。

---

## 五、待实现的功能

### P0 — 并行节点并发执行

`ParallelNode` 当前为顺序执行（serial fallback）。升级为 `tokio::join!` 真正的并发执行：

```rust
// 当前：顺序执行分支
for (name, node) in &self.branches { ... }

// 目标：并发执行
let handles = self.branches.iter().map(|(name, node)| {
    tokio::spawn(async move { ... })
});
let results = futures::join!(handles...);
```

### P1 — 子图 / 嵌套 Graph

`Graph::run_inline()` 已实现，需添加 `NodeKind::Graph(sub_graph)` 变体：

```rust
let sub_graph = build_research_subgraph();
GraphBuilder::new("pipeline")
    .node("research", NodeKind::Graph(sub_graph))
    .node("write", NodeKind::Task(...))
    .edge("research", "write")
    .build();
```

### P2 — Checkpoint 恢复 API

`CheckpointStore` 已实现，需添加 Resume API：

```rust
// 设想 API
let checkpoint = executor.checkpoint(&graph, &state)?;
// ... 进程重启 ...
let (result, state) = GraphExecutor::resume(checkpoint, graph).await?;
```

### P3 — Memory / 长期记忆

超越 `ContextBudget` + `LocalCompactor` 的上下文压缩，实现跨会话的语义记忆：

```rust
AgentBuilder::new(model)
    .memory(SemanticMemory::new(vector_store).with_window(10))
    .build();
```

---

## 六、执行流程图

### LeLLM GraphExecutor::run_loop 主循环

```
execute_stream()
  └── tokio::spawn(run_loop)
        │
        ▼
    GraphStart ──────────────────────────────────────────┐
        │                                                 │
        ▼                                                 │
    [cancel?] ──yes→ GraphError (BarrierCancelled)       │
        │ no                                              │
        ▼                                                 │
    [max_steps?] ──yes→ GraphError (StepsExceeded)       │
        │ no                                              │
        ▼                                                 │
    NodeStart                                             │
        │                                                 │
        ▼                                                 │
    execute_node(node, state, ...)                        │
        │ 返回 (NextAction, Signal, Metadata, FlowEvents) │
        ▼                                                 │
    [Signal::Pause?] ──yes→ handle_barrier_signal() ──┐  │
        │ no                                          │  │
        ▼                                             │  │
    [NextAction]                                      │  │
      End  → GraphComplete                            │  │
      Goto → current = target ────────────────────────┘  │
      Next → resolve_next(graph, current, state)         │
        ├─ 条件边 (first-match-wins)                     │
        ├─ 普通边                                        │
        └─ Fallback 边                                   │
           → current = target ───────────────────────────┘
        │                                                 │
        ▼                                                 │
    NodeEnd + Checkpoint (if policy matches)              │
        │                                                 │
        └─────────────────────────────────────────────────┘
                                                 │
                                                 ▼
                                            GraphComplete / GraphError
```

---

## 附录：示例对照

完整示例代码见 `lellm-graph/examples/calculator_graph.rs`，可直接运行：

```bash
cargo run -p lellm-graph --example calculator_graph
```
