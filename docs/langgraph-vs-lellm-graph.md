# LangGraph 与 LeLLM Graph 设计对比

> 基于 LangGraph 官方 Quickstart Tutorial，对比分析两种编排架构的设计取舍。

## 核心对撞：微观自由度 vs 宏观确定性

LangGraph（以 Python/JS 为代表的动态语言图生态）与 LeLLM（以 Rust 为代表的系统级类型图生态）在架构哲学上存在根本性分水岭。这个差异不仅体现在控制流和状态管理的底层实现上，更决定了框架的抽象颗粒度。

---

## 一、功能对照表

| 维度 | LangGraph (Python/JS) | LeLLM (Rust) |
|------|----------------------|--------------|
| **状态管理** | `TypedDict` / `zod` schema + `operator.add` reducer | `HashMap<String, serde_json::Value>` — 扁平 KV，reducer 需手动在 TaskNode 中实现 |
| **工具定义** | `@tool` decorator / `tool()` + zod schema | `#[derive(Tool)]` macro + `ToolRegistration` |
| **Agent 循环** | 手动构建 `llm_node → tool_node → condition → back` | `ToolUseLoop` 内置 ReAct 循环，无需手动构建 |
| **条件路由** | `should_continue()` 函数返回 node name / `END` | `ConditionNode` 声明式分支 + `edge_if()` 条件边 |
| **图构建** | `StateGraph.add_node().add_edge().compile()` | `GraphBuilder.node().edge().build()` |
| **执行** | `agent.invoke({messages: [...]})` | `GraphExecutor::execute(&graph, state)` |
| **循环支持** | 天然支持（边可回环） | DAG + `LoopNode` 显式循环（构建时环检测） |

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
// 显式追加消息
let messages: Vec<Message> = state.get("messages")...;
messages.push(new_msg);
state.insert("messages".into(), serde_json::to_value(&messages)?);
```

**优势：** 显式高于隐式。哪个节点、在什么时候、以什么规则把数据 merge 回主 State，在编译期被写得一清二楚，且绝对不会发生线程竞态（Data Race）。

---

### 3. 循环支持：自由拓扑回环 vs DAG + 结构化 LoopNode

这是整个图引擎设计中最高阶的架构权衡。

#### LangGraph — 自由拓扑回环

LangGraph 的底层引擎是一个纯粹的 Stateful State Machine（带状态机路由）。通过条件路由函数 `should_continue` 返回字符串形式的节点名，控制流可以任意回溯到图的任何角落。

```python
def should_continue(state: MessagesState) -> Literal["tool_node", END]:
    if state["messages"][-1].tool_calls:
        return "tool_node"  # 回溯到 tool_node
    return END
```

**代价：** 图的拓扑校验（Validation）基本失效。在运行前，你无法通过静态算法发现这张图是否会产生死循环、是否有孤立节点（Dead End）、或者状态机是否会跳转到一个不存在的节点名。所有的错误都只能推迟到运行时通过崩溃来暴露。

#### LeLLM — DAG + 结构化 LoopNode

LeLLM 采用了更现代、更安全的工业工作流设计：图的骨架必须是严格的有向无环图（DAG），在构建期（`GraphBuilder::build()`）直接进行强力的拓扑环检测（DFS 染色）。

```rust
// 构建时即检测环
let graph = GraphBuilder::new("workflow")
    .start("a")
    .node("a", ...)
    .node("b", ...)
    .edge("a", "b")
    .edge("b", "a")  // ❌ build() 直接报错：cycle detected
    .end("b")
    .build();
```

如果需要循环，不能在拓扑层乱拉回头边，必须显式地封装进一个带有 `max_iterations` 熔断器的 `LoopNode` 容器中。

**优势：** 把死循环和脑裂的风险死死扼杀在编译期/构建期。

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

用 `LoopNode` 代替拓扑回环，配合 `ConditionNode` 声明式分支，使得 Agent 复杂的思考轨迹（Trajectory）变得完全可预测、可追踪、可校验。

### 3. 类型安全

Rust 的强类型系统 + `#[derive(Tool)]` 编译期 schema 生成，确保工具签名、参数校验、错误处理全部在编译期完成，而非运行时动态推断。

### 4. 构建期校验

DAG 骨架 + 环检测 + 节点/边存在性校验，所有图结构问题在 `build()` 时暴露，而非运行时崩溃。

---

## 五、改进方向

基于对比分析，以下增强可以让 LeLLM Graph 更加完善：

### P0 — AgentNode 状态暴露

当前 `AgentNode` 仅将最终文本写入 `output` key。应扩展为：
- 将 `ToolUseResult.messages` 写回 State，让后续节点可访问完整对话历史
- 在 State 中写入 `iterations`、`tool_calls_executed` 等执行统计
- 可配置的消息 key / 输出 key

### P1 — 状态 Reducer 支持

引入可选的 reducer 机制，类似 LangGraph 的 `operator.add`，但保持显式：
```rust
// 节点声明需要追加的 key
state.append("messages", new_messages, |existing, new| {
    let mut msgs: Vec<Message> = serde_json::from_value(existing.clone())?;
    msgs.extend(serde_json::from_value(new.clone())?);
    Ok(serde_json::to_value(msgs)?)
});
```

### P2 — 流式事件穿透

`AgentNode` 支持流式发射事件到 Graph 层：
```rust
let result = GraphExecutor::execute_stream(&graph, state, event_sink).await;
```

### P3 — 细粒度 LLMNode + ToolNode

为需要完全控制 Agent Loop 的用户，提供"手动模式"：
- `LLMNode` — 单次 LLM 调用，将结果写入 State
- `ToolNode` — 读取 State 中的 tool_calls，执行工具，返回结果
- 用户可用 `ConditionNode` + `edge_if()` 手动构建 ReAct 循环

---

## 附录：示例对照

完整示例代码见 `lellm-graph/examples/calculator_graph.rs`，可直接运行：

```bash
cargo run -p lellm-graph --example calculator_graph
```
