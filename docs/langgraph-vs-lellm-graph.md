# LangGraph 与 LeLLM Graph 设计对比

> 基于 LangGraph 官方 Quickstart Tutorial，对比两种编排架构的核心差异。
> 更新于 2026-06-25，与 v0.4 代码深度对齐。

---

## 一、功能对照表

| 维度 | LangGraph (Python/JS) | LeLLM (Rust) |
|------|----------------------|--------------|
| **状态管理** | `TypedDict` + `operator.add` reducer | 双层：`State`（`HashMap<String, Value>`）+ `WorkflowState` trait（编译期类型安全，Effect 驱动）；`StateKey<T>` 编译期类型安全的键；`BranchState` Overlay 模型 |
| **状态变更** | 节点直接修改 State dict | `StateEffect` 领域事件 + `NodeContext::emit_effect()` → Executor 批量 `apply_batch()`；节点不直接写 State |
| **工具定义** | `@tool` decorator + zod schema | `#[derive(Tool)]` macro + `ToolRegistration` + `ToolCatalog` 动态发现 |
| **Agent 循环** | 手动构建 `llm_node → tool_node → condition → back` | 双模式：(1) `ToolUseLoop` 内置 ReAct 循环（黑盒）；(2) 内部构建 `Graph<AgentState>` ReAct 有环图（LLMNode → BudgetCondition → CompactorNode → ToolNode） |
| **条件路由** | `should_continue()` 函数返回 node name | `ConditionNode` 声明式分支 + `edge_if()` 条件边（first-match-wins）；三元优先级：条件边 > 普通边 > Fallback 边 |
| **图构建** | `StateGraph.add_node().add_edge().compile()` (fail-fast) | `GraphBuilder.node().edge().build()` (多错误收集，返回 `BuildErrors`) |
| **执行** | `agent.invoke({messages: [...]})` | `GraphExecutor::execute()` (阻塞) / `execute_stream()` (流式，返回 `GraphExecution{stream, handle}`) / `run_inline()` (内联子图) |
| **循环支持** | 天然支持（边可回环） | 允许有环图 + `max_steps` 运行时熔断 + `CycleAnalysis` 静态诊断 + `max_visits` 边级保护 |
| **错误分类** | Exception 机制 | 二分法：`TerminalError`（不可恢复）/ `ObservedError`（可观测，不影响控制流）；Fallback 通过 `edge_fallback()` 构建期配置 |
| **Human-in-the-loop** | `InterruptBefore` + `update_state` / `restart` | `BarrierNode` + `GraphHandle::decide()` — Approve/Reject/Modify/Reroute + 超时 + wildcard 决策 |
| **流式输出** | `astream()` 返回检查点变更事件 | `GraphStream` (`mpsc::Receiver<GraphEvent>`) — 全链路事件 + TraceId/SpanId + `RuntimeEvent` 可观测层 |
| **持久化** | Checkpointing（SQLite/PostgreSQL/Shallow） | `CheckpointStore` trait + `Checkpoint<S>` 泛型 + `CheckpointPolicy` (EveryNode/BarrierOnly/Manual) |
| **并行节点** | `Send()` + 并行执行 | `ParallelNode<S, M>` + `MergeStrategy<S>` trait + `ParallelNodeBuilder`（当前顺序执行，API 完备）|
| **子图/嵌套** | 支持（`StateGraph` 可嵌套） | `Graph::run_inline()` 内联执行子图 + `NodeContext` 传递控制信号 |
| **节点扩展** | 自定义 node 函数 | `NodeKind` 枚举（Task/Condition/Barrier/Parallel/External）+ `FlowNode<S>` trait |

---

## 二、核心架构差异

### 1. Agent 循环：LangGraph 的"平铺" vs LeLLM 的"三层封装"

**LangGraph — 拓扑层展开**

工具循环在图的拓扑层直接展开。一个简单的 ReAct 智能体需要 3 个节点和 2~3 条边。加入宏观流程后，图的连接会变成蜘蛛网。

```
START → llm_node → tool_node → (should_continue) → llm_node → ... → END
```

**LeLLM — 三层封装**

**第一层：`ToolUseLoop`（黑盒 ReAct）** — LLM ↔ Tools 流式交互封装为独立循环，含 Retry/Fallback/ContextCompactor。

```
init → AgentNode(内部: ToolUseLoop) → summary → END
              ┌─────────────────────┐
              │ LLM → Tools → LLM   │  ← 自动循环
              │ (含 Retry/Fallback) │
              └─────────────────────┘
```

**第二层：`Graph<AgentState>` 内部有环图** — ReAct 循环展开为强类型图（零序列化）：

```
budget_check → llm → post_llm_check → (has_tool_calls?) → tool → budget_check
    ↑                                                    │
    └──── compactor ← (budget_exceeded)                   └──── (no_calls) → end
```

**第三层：宏观 Graph 编排** — `AgentFlowNode` 作为 `NodeKind::External` 嵌入宏观 Graph。

**优势：** 图只关心节点的输入输出 State（通过 `StateEffect` 声明式写入），天然支持 Multi-Agent 层次化网络。

---

### 2. 状态管理：Effect 驱动 + Overlay 模型

**LangGraph — 隐式 Reducer**

```python
class MessagesState(TypedDict):
    messages: Annotated[list[AnyMessage], operator.add]
```

多个节点返回数据时自动隐式合并。在复杂并发场景下极易触发黑盒 Bug。

**LeLLM — Effect 驱动 + BranchState Overlay**

```rust
// 节点通过 Effect 声明状态变更意图（不直接写 State）
ctx.emit_effect(StateEffect::Put("messages", json!(messages)));
ctx.emit_effect(StateEffect::Delete("temp_key"));

// 类型化 State 使用自定义 Effect enum（零序列化）
ctx.emit_effect(AgentEffect::AppendMessage(msg));

// Executor 统一消费 Effects → apply 到 typed state
let effects = ctx.consume_effects();
ctx.state_mut().apply_batch(effects);
```

**BranchState — Overlay 模型：**

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
```

**MergeStrategy — 并行合并职责剥离：**

合并规则由 Graph 层的 `MergeStrategy<S>` 决定，而非 State 内建属性。`StateMerge`（默认，逐 key 合并）、`LastWriteWins`（最后一个分支整体获胜）、自定义策略。

---

### 3. 控制流：NextStep + ExecutionSignal 正交分离

**LangGraph — 节点返回值决定路由**

```python
def my_node(state):
    result = do_something(state)
    if result.needs_review:
        return {"next": "review"}
    return {"next": "done"}
```

路由逻辑和执行逻辑耦合在同一个函数中。

**LeLLM — 正交分离**

```rust
// 1. NextStep — 拓扑路由（GoToNext/Goto/End）
pub enum NextStep {
    GoToNext,    // 按拓扑顺序走下一步
    Goto(String), // 跳转到指定节点
    End,         // 结束执行
}

// 2. ExecutionSignal — 运行时信号（Barrier 挂起等）
pub enum ExecutionSignal {
    Pause { barrier_id: BarrierId, timeout: Option<Duration> },
}

// 节点通过 NodeContext 写入控制信号
ctx.goto("review");        // → NextStep::Goto
ctx.end();                 // → NextStep::End
ctx.pause(barrier_id);     // → ExecutionSignal::Pause

// Executor 统一读取
let (next_step, signal) = ctx.take_control();
```

**边解析的三元优先级：** 条件边（`edge_if`，first-match-wins）> 普通边（`edge`）> Fallback 边（`edge_fallback`）

---

### 4. 循环安全：自由拓扑回环 vs 多层防护

**LangGraph — 自由拓扑回环**

通过条件路由函数返回字符串形式的节点名，控制流可以任意回溯。代价：拓扑校验基本失效，死循环、孤立节点只能在运行时暴露。

**LeLLM — 四层防护：**

```rust
// 第 1 层：max_steps 运行时熔断（默认 50）
GraphExecutor::new(50).execute(graph, state)?;

// 第 2 层：max_visits 边级保护
g.edge("retry", "agent").max_visits(5);

// 第 3 层：CycleAnalysis 静态诊断
let analysis = graph.analyze_cycles();
// "Cycle 1: agent → retry → agent — ⚠️ UNPROTECTED"

// 第 4 层：GraphDiagnostics 综合诊断
let diag = graph.analyze();
// 检测：环、不可达节点、end 节点出边、Fallback 在环中的位置
```

---

### 5. 错误处理：Exception vs 二分法 + Fallback

**LangGraph — Exception**

依赖 Python 的 exception 机制，未捕获异常中断图执行。

**LeLLM — 二分法 + Fallback 控制流**

```rust
// GraphError — 只有 Terminal 变体
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
g.edge_fallback("agent", "degraded_mode");  // agent 失败 → 降级模式
```

---

### 6. Human-in-the-loop：中断恢复 vs Barrier 决策

**LangGraph**

通过 `InterruptBefore` 在指定节点前中断，然后调用 `update_state()` 修改状态或 `restart()` 继续。机制底层，需手动管理中。

**LeLLM — BarrierNode 结构化审批**

```rust
// 定义 Barrier 节点
let barrier = BarrierNode::new("approval")
    .timeout(Duration::from_secs(300))
    .default_action(BarrierDefaultAction::Reject)
    .reject_key("rejected")
    .approve_key("approved");

// 图中使用（BarrierNode<S> 直接传入，无需 Box）
g.node("approval", NodeKind::Barrier(barrier));

// 执行时通过 Handle 提交决策（async）
handle.decide(barrier_id, BarrierDecision::Approve).await?;
handle.decide(barrier_id, BarrierDecision::Reject { reason: "..." }).await?;
handle.decide(barrier_id, BarrierDecision::Modify { key: "result", value: ... }).await?;
handle.decide(barrier_id, BarrierDecision::Reroute { target: "review" }).await?;

// wildcard 决策 — 对指定 node_id 的 Barrier 统一处理
handle.decide_wildcard("approval", BarrierDecision::Approve).await?;

// 取消执行
handle.cancel();
```

**决策机制：** `DecisionRegistry` 采用 level-triggered 设计，提前提交的决策被缓存（`pending` HashMap + `wildcards`）。`BarrierId` 由 `node_id` + `occurrence` 组成，支持同一 Barrier 多次触发。

---

### 7. 并行执行：ParallelNode + MergeStrategy

```rust
// 构建 ParallelNode（默认 StateMerge — 逐 key 合并）
let parallel = ParallelNode::builder()
    .branch("search_web", search_node)
    .branch("search_docs", docs_node)
    .branch("search_code", code_node)
    .error_strategy(ParallelErrorStrategy::FailFast)
    .build();

// 切换合并策略（类型方法，返回新类型构建器）
let parallel = ParallelNode::builder()
    .branch("search_web", search_node)
    .merge_strategy::<LastWriteWins>()  // 最后一个分支获胜
    .build();

// 图中使用
g.node("research", NodeKind::Parallel(parallel));
```

**执行流程：** 克隆 base State → 每个分支独立 `BranchState` → 分支执行 → 消费 Effects → `MergeStrategy::merge(branches)` 合并 → 替换父 State。

当前为顺序执行（serial fallback），API 层已完备，可升级为 `tokio::join!` 并行。

---

### 8. 持久化 / Checkpointing

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

**设计原则：** Checkpoint 唯一职责是恢复（Restore）。给我一个 Checkpoint 文件就能从 `current_node` 继续执行。

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

## 四、LeLLM 的工程优越性

### 1. 宏观极简，微观极速

`ToolUseLoop` 坍缩到节点内部，避免图拓扑污染。结合 Tokio 原生异步，吞吐高出动态语言几个数量级。

### 2. Effect 驱动状态变更

节点通过 `emit_effect()` 声明意图，Executor 统一 `apply_batch()`。`BranchState` Overlay 模型提供增量审计。

### 3. 编译期类型安全

`WorkflowState` trait + `StateKey<T>` + `MergeStrategy<S>` 构成编译期类型安全三角。`Graph<AgentState>` 零序列化。

### 4. 正交控制流

`NextStep`（拓扑路由）、`ExecutionSignal`（运行时信号）、`NodeMetadata`（元数据）三者解耦。Barrier 挂起不污染路由。

### 5. Stream-First 设计

`execute()` 内部消费 `execute_stream()` — 流式为首要模式，阻塞为派生。全链路 `GraphEvent` 配合 `TraceId`/`SpanId` 贯穿。独立 `RuntimeEvent` enum（`ExecutionStarted`/`NodeStarted`/`NodeCompleted`/`NodeFailed`/`BarrierWaiting`/`BarrierResolved`/`ExecutionCompleted`）提供可观测性钩子。

### 6. 构建期多错误收集

`build()` 一次性收集所有错误后统一报告，返回 `Result<Graph, BuildErrors>`。`GraphDiagnostics` 提供非致命诊断。LangGraph 的 `compile()` 是 fail-fast，需多次修正、多次编译。

### 7. Human-in-the-loop 一等公民

`BarrierNode` 提供结构化审批决策（Approve/Reject/Modify/Reroute），配合 `DecisionRegistry` 的 level-triggered 机制和 wildcard 决策。

---

## 五、执行流程

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
        │ 返回 (NextStep, Signal, Metadata, FlowEvents)   │
        ▼                                                 │
    [Signal::Pause?] ──yes→ handle_barrier_signal() ──┐  │
        │ no                                          │  │
        ▼                                             │  │
    [NextStep]                                        │  │
      End  → GraphComplete                            │  │
      Goto → current = target ────────────────────────┘  │
      GoToNext → resolve_next(graph, current, state)     │
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

完整示例代码见 `lellm-agent/examples/calculator_graph.rs`，可直接运行：

```bash
# OpenAI 兼容 API:
OPENAI_API_KEY=sk-xxx cargo run -p lellm-agent --example calculator_graph

# 或 Ollama:
OPENAI_API_BASE=http://localhost:11434/v1 OPENAI_API_KEY=ollama \
  cargo run -p lellm-agent --example calculator_graph
```
