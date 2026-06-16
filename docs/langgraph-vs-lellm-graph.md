# LangGraph 与 LeLLM Graph 设计对比

> 基于 LangGraph 官方 Quickstart Tutorial，对比分析两种编排架构的设计取舍。
> 更新于 2026-06-16，与 v0.2.0 代码对齐。

## 核心对撞：微观自由度 vs 宏观确定性

LangGraph（以 Python/JS 为代表的动态语言图生态）与 LeLLM（以 Rust 为代表的系统级类型图生态）在架构哲学上存在根本性分水岭。这个差异不仅体现在控制流和状态管理的底层实现上，更决定了框架的抽象颗粒度。

---

## 一、功能对照表

| 维度 | LangGraph (Python/JS) | LeLLM (Rust) |
|------|----------------------|--------------|
| **状态管理** | `TypedDict` / `zod` schema + `operator.add` reducer | `HashMap<String, serde_json::Value>` — 扁平 KV；`State::reduce()` + `array_reducer()` 内置 reducer；`StateKey<T>` 编译期类型安全 |
| **工具定义** | `@tool` decorator / `tool()` + zod schema | `#[derive(Tool)]` macro + `ToolRegistration` + `ToolCatalog` 动态发现 |
| **Agent 循环** | 手动构建 `llm_node → tool_node → condition → back` | `ToolUseLoop` 内置 ReAct 循环；也可用 `LLMNode` + `ToolNode` + `ConditionNode` 手动构建 |
| **条件路由** | `should_continue()` 函数返回 node name / `END` | `ConditionNode` 声明式分支 + `edge_if()` 条件边（first-match-wins）|
| **图构建** | `StateGraph.add_node().add_edge().compile()` (fail-fast) | `GraphBuilder.node().edge().build()` (多错误收集，返回 `BuildErrors`) |
| **构建校验** | fail-fast（遇错即停） | 多错误收集 + `Warning` 非致命变体 + 重复节点检测 |
| **执行** | `agent.invoke({messages: [...]})` | `GraphExecutor::execute()` (阻塞) / `execute_stream()` (流式，stream-first) |
| **循环支持** | 天然支持（边可回环） | 允许有环图 + `max_steps` 运行时熔断 + `CycleAnalysis` 静态诊断 |
| **错误分类** | Exception 机制 | 三分法：`TerminalError`（不可恢复）/ `RecoverableError`（触发 fallback 边）/ `ObservedError`（可观测，不影响控制流）|
| **Human-in-the-loop** | `InterruptBefore` + `update_state` / `restart` | `BarrierNode` + `GraphHandle::decide()` — 支持 Approve/Reject/Modify/Reroute + 超时 + wildcard 决策 |
| **流式输出** | `astream()` 返回检查点变更事件 | `GraphStream` — 全链路事件（NodeStart/End, AgentEvent, BarrierWaiting/Resolved）+ TraceId/SpanId |
| **持久化** | Checkpointing（SQLite/PostgreSQL/Shallow） | **未实现** |
| **并行节点** | `Send()` + 并行执行 | **未实现**（Graph 层顺序执行；AgentNode 内部工具可并发）|
| **子图/嵌套** | 支持（`StateGraph` 可嵌套） | **未实现** |

---

## 二、核心架构差异深度分析

### 1. Agent 循环：LangGraph 的"平铺" vs LeLLM 的"黑盒封装"

#### LangGraph — 拓扑层展开

在 LangGraph 里，工具循环（Tool Loop）是在图的拓扑层（Topological Layer）直接展开的。大模型吐出 Tool Call，图导航到 `tool_node`，执行完再路由回 `llm_node`。

```
START → llm_node → tool_node → (should_continue) → llm_node → ... → END
```

**代价：** 图的规模急剧膨胀。一个简单的 ReAct 智能体，在图里就需要 3 个节点和 2~3 条边。如果想在图里加入更宏观的流程（例如：先规划，再执行 ReAct，最后审查），整张图的 DAG 连接会变成密密麻麻的蜘蛛网，极其难以维护。

#### LeLLM — 节点内封装

LeLLM 贯彻的是"局部高内聚，宏观低耦合"的六边形架构。大模型与工具之间频繁的、带有 `RetryPolicy` 的流式交互（Streaming Loop），本就不该打扰宏观控制流。

```
init → AgentNode(内部: ToolUseLoop) → summary → END
              ┌─────────────────────┐
              │ LLM → Tools → LLM   │  ← 自动循环
              │ (含 Retry/Fallback) │
              └─────────────────────┘
```

**优势：** 在 LeLLM 的 Graph 层，一个极其复杂的 AI Agent（包含完整的 ReAct 机制）在图里仅仅缩写为一个标准的 `AgentNode`。图只需要关心这个 Node 的输入 State 和输出 State。这使得 LeLLM 天然具备构建 **Multi-Agent 层次化网络（Hierarchical Agent Networks）** 的顶级底座能力。

**手动模式：** 对需要完全控制 Agent Loop 的用户，LeLLM 提供 `LLMNode`（单次 LLM 调用）+ `ToolNode`（工具执行）+ `ConditionNode`（条件分支），可用 `edge_if()` 手动构建任意 ReAct 循环——与 LangGraph 的粒度对等。

---

### 2. 状态管理：动态 Reducer 语义 vs 显式强合并

#### LangGraph — 隐式 Reducer

Python 的动态特性允许用户在全局 State 的某个 Key 上绑定一个隐式的 reducer 闭包：

```python
class MessagesState(TypedDict):
    messages: Annotated[list[AnyMessage], operator.add]
```

当多个节点返回数据时，LangGraph 自动在后台进行隐式合并。

**痛点：** 缺乏静态可预测性。在复杂的并发场景或 Parallel Node 汇聚时，这种隐式的、跨越多个节点的 State 合并极易触发黑盒 Bug（比如 Message 列表顺序由于异步竞争发生错乱）。

#### LeLLM — 显式操作

Rust 作为强类型系统语言，拒绝这种"中途拦截自动修改"的黑魔法。LeLLM 使用扁平的 `HashMap` 传递 State，节点通过 `&mut State` 显式读写。

```rust
// 方式一：显式追加消息
let messages: Vec<Message> = state.require("messages")?;
messages.push(new_msg);
state.set("messages", &messages);

// 方式二：Reducer 机制（类似 LangGraph operator.add，但显式声明）
state.reduce("messages", &new_msgs, array_reducer)?;

// 方式三：StateKey — 编译期类型安全的键
use lellm_graph::statekey::SK_MESSAGES;
state.set_sk(SK_MESSAGES, &messages);
let msgs: &Vec<Message> = state.require_sk(SK_MESSAGES)?;
```

**优势：** 显式高于隐式。哪个节点、在什么时候、以什么规则把数据 merge 回主 State，在编译期被写得一清二楚，且绝对不会发生线程竞态（Data Race）。

---

### 3. 循环支持：自由拓扑回环 vs 有环图 + 运行时熔断

这是整个图引擎设计中最高阶的架构权衡。

#### LangGraph — 自由拓扑回环

LangGraph 的底层引擎是一个纯粹的 Stateful State Machine（带状态机路由）。通过条件路由函数 `should_continue` 返回字符串形式的节点名，控制流可以任意回溯到图的任何角落。

```python
def should_continue(state: MessagesState) -> Literal["tool_node", END]:
    if state["messages"][-1].tool_calls:
        return "tool_node"  // 回溯到 tool_node
    return END
```

**代价：** 图的拓扑校验（Validation）基本失效。在运行前，你无法通过静态算法发现这张图是否会产生死循环、是否有孤立节点（Dead End）、或者状态机是否会跳转到一个不存在的节点名。所有的错误都只能推迟到运行时通过崩溃来暴露。

#### LeLLM — 有环图 + 运行时熔断

LeLLM 允许图中存在环（不强制 DAG），通过多层防护确保安全：

```rust
// 环是允许的 — 用于实现重试、迭代等模式
let graph = GraphBuilder::new("workflow")
    .start("agent")
    .node("agent", NodeKind::Agent(...))
    .node("retry", NodeKind::Task(...))
    .edge_if("agent", "retry", |s| s.get_bool("should_retry").unwrap_or(false))
    .edge("agent", "summary")
    .edge("retry", "agent")  // ✅ 允许回头边
    .end("summary")
    .build()?;

// 运行时熔断：超过 max_steps 自动终止
let (result, state) = GraphExecutor::new(50)  // 最多 50 步
    .execute(graph, state)?;
// 返回 TerminalError::StepsExceeded 而非死循环
```

同时提供 `CycleAnalysis` 静态诊断（DFS 环检测 + 诊断报告），帮助开发者在开发期理解图的循环结构。

**优势：** 既保留了 LangGraph 式的灵活回环能力，又通过运行时熔断兜底防止无限循环。

---

### 4. 错误处理：Exception vs 三分法

#### LangGraph — Exception

LangGraph 依赖 Python 的 exception 机制。任何未捕获的异常都会中断图执行，需要外部 try/catch 处理。

#### LeLLM — 三分法

LeLLM 将错误分为三个正交的类别：

```rust
// 1. TerminalError — 不可恢复，终止执行，stream 关闭
//    变体：InvalidGraph, NodeNotFound, MissingEdge, NodeExecutionFailed,
//          StepsExceeded, Unrouted, StateError, BarrierTimeout...

// 2. RecoverableError — 可恢复，触发 fallback 边
//    变体：Retryable{node, attempt, max_attempts, reason},
//          FallbackTriggered{from, to, reason}
//    配合 edge_fallback() 实现降级路由：
let graph = GraphBuilder::new("resilient")
    .edge_fallback("agent", "degraded_mode")  // agent 失败 → 降级模式
    .build()?;

// 3. ObservedError — 可观测性事件，不影响控制流
//    变体：Warning, Degraded, PartialFailure
//    发射 GraphEvent::ObservedError，执行继续
```

---

### 5. Human-in-the-loop：中断恢复 vs Barrier 决策

#### LangGraph

通过 `InterruptBefore` 在指定节点前中断，然后调用 `update_state()` 修改状态或 `restart()` 继续执行。机制较为底层，需要手动管理中断点。

#### LeLLM

提供 `BarrierNode` 作为专用的审批节点，配合 `GraphHandle::decide()` 提交结构化决策：

```rust
// 定义 Barrier 节点
let barrier = BarrierNode::new("approval")
    .timeout(Duration::from_secs(300))
    .default_action(BarrierDecision::Reject { reason: "超时未审批" })
    .reject_key("rejected")
    .approve_key("approved");

// 图中使用
.node("approval", NodeKind::Barrier(Box::new(barrier)))

// 执行时通过 Handle 提交决策
handle.decide(barrier_id, BarrierDecision::Approve)?;
handle.decide(barrier_id, BarrierDecision::Modify { key: "result", value: ... })?;
handle.decide(barrier_id, BarrierDecision::Reroute { target: "review" })?;
// 支持 wildcard 决策 — 对未指定 barrier_id 的 Barrier 统一处理
handle.decide_wildcard(BarrierDecision::Approve)?;
```

仅支持流式模式。决策采用 level-triggered 机制（提前提交的决策会被保留）。

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

### 2. 结构化控制流

有环图 + `max_steps` 熔断 + `ConditionNode` 声明式分支 + 三分法错误处理，使得 Agent 复杂的思考轨迹（Trajectory）变得完全可预测、可追踪、可校验。

### 3. 类型安全

Rust 的强类型系统 + `#[derive(Tool)]` 编译期 schema 生成 + `StateKey<T>` 编译期类型安全的状态键，确保工具签名、参数校验、错误处理、状态访问全部在编译期完成，而非运行时动态推断。

### 4. Stream-First 设计

`execute()` 内部消费 `execute_stream()` 的流 — 流式是首要的执行模式，阻塞模式是派生。全链路 `GraphEvent` 配合 `TraceId`/`SpanId` 贯穿，开箱即用的可观测性。

### 5. 构建期校验 — 多错误收集

`build()` 一次性收集所有错误后统一报告（而非遇到第一个错误就停止），返回 `Result<Graph, BuildErrors>`：

```rust
// 多错误收集 — 所有问题一次性暴露
match builder.build() {
    Ok(graph) => { /* 使用 graph */ }
    Err(errors) => {
        // 可能包含 MissingNode × 3, DuplicateNode × 1, Warning × 2
        for e in &errors.0 {
            eprintln!("{}", e);
        }
    }
}
```

`BuildError::Warning` 是非致命变体（如多条件边警告、重复节点名），不阻止构建成功。致命错误（`MissingNode`, `MissingEntryPoint` 等）才导致 `build()` 失败。

LangGraph 的 `compile()` 是 fail-fast（遇到第一个错误就抛异常），开发者需要多次修正、多次编译。LeLLM 的多错误收集减少了 edit-compile 循环。

### 6. Human-in-the-loop 一等公民

`BarrierNode` 提供结构化的审批决策（Approve/Reject/Modify/Reroute），而非 LangGraph 式的底层中断+手动恢复。

---

## 五、待实现的功能

### P0 — 持久化 / Checkpointing

LangGraph 的核心竞争力之一。实现图执行状态的序列化/反序列化，支持中断恢复：

```rust
// 设想 API
let checkpoint = executor.checkpoint(&graph, &state)?;
// ... 进程重启 ...
let (result, state) = GraphExecutor::resume(checkpoint, graph)?;
```

### P1 — 并行节点执行

LangGraph 通过 `Send()` 实现节点级并行。LeLLM 目前仅在 AgentNode 内部（工具执行层）支持并发：

```rust
// 设想 API
GraphBuilder::new("parallel")
    .parallel("research", vec!["search_web", "search_docs", "search_code"])
    .node("synthesize", ...)
    .edge("research", "synthesize")
    .build()?;
```

### P2 — 子图 / 嵌套 Graph

将 Graph 作为节点嵌入另一层 Graph，实现模块化编排：

```rust
// 设想 API
let sub_graph = build_research_subgraph();
GraphBuilder::new("pipeline")
    .node("research", NodeKind::Graph(sub_graph))
    .node("write", ...)
    .edge("research", "write")
    .build()?;
```

### P3 — Memory / 长期记忆

超越 `ContextBudget` + `LocalCompactor` 的上下文压缩，实现跨会话的语义记忆：

```rust
// 设想 API
AgentBuilder::new(model)
    .memory(SemanticMemory::new(vector_store).with_window(10))
    .build();
```

---

## 附录：示例对照

完整示例代码见 `lellm-graph/examples/calculator_graph.rs`，可直接运行：

```bash
cargo run -p lellm-graph --example calculator_graph
```
