# v0.5 架构重构：Graph is Runtime, Agent is DSL

> 日期：2026-06-30
> 状态：设计方案 v2，已整合评审反馈

## 核心论点

**Graph 是唯一的 Runtime，Agent 是 Graph 的 DSL / 模板构建器（Graph Factory）。**

```
用户层
──────────────────────────────────────
AgentBuilder  ReactBuilder  PlannerBuilder  SupervisorBuilder
                        (DSL / Graph Factory, impl GraphFactory<AgentState>)

                        ↓ build() →

                   Graph<AgentState>

                   ┌──────────────────────┐
                   │  ToolUseLoop (Facade) │  ← 薄层，持有 Graph + 高级 API
                   │  invoke()             │     invoke_stream(), invoke_json()
                   │  invoke_stream()      │
                   │  invoke_json()        │
                   └──────────────────────┘

──────────────────────────────────────
          Runtime (lellm-graph)
ExecutionEngine  Node  Edge  State  Graph

──────────────────────────────────────
          Primitive (lellm-graph)
GraphBuilder  (AST 构建器，compile() → immutable Graph)
```

类比：`Router::new().route().layer().build()` 生成 Axum `Router`；
`AgentBuilder::new().tools().build()` 生成 `Graph<AgentState>`。

## 当前问题

### 1. 双重 Runtime

```
AgentBuilder → ToolUseLoop → build_react_graph() → Graph<AgentState> → ExecutionEngine
                                         ↓
                                    (另一个 ExecutionEngine)

外层: GraphBuilder → Graph<State> → ExecutionEngine
        ↓ contains
   AgentFlowNode (External node)
        ↓ internally calls
   ToolUseLoop.execute() → build_react_graph() → Graph<AgentState> → ExecutionEngine
```

问题：
- **两个 ExecutionEngine** 在跑 —— Checkpoint、Trace、Cancellation、Streaming 全部双层
- **AgentFlowNode** 是 graph-in-graph 的反模式，递归执行
- **ToolUseLoop** 是一个独立的运行时实体，不是 graph 的一等公民

### 2. AgentBuilder 的黑盒性质

`AgentBuilder::build()` → `ToolUseLoop`（一个不透明的执行循环）

用户看不到 ReAct loop 的内部结构，也无法定制。这违背了"让开发者精准控制 Agent 的执行流程"。

### 3. 心智模型混乱

用户需要同时理解两套 API：
- `GraphBuilder` — "建图"
- `AgentBuilder` — "建 Agent"

但 LangGraph 用户的心智模型只有一套：`StateGraph().add_node().add_edge().compile()`

## 目标架构

### 不变的部分

| 组件 | 位置 | 说明 |
|------|------|------|
| `GraphBuilder` | `lellm-graph` | 原语，不动 |
| `Graph<S, M>` | `lellm-graph` | 唯一的图结构 |
| `ExecutionEngine` | `lellm-graph` | 唯一的执行引擎 |
| `NodeKind` / `Edge` / `State` | `lellm-graph` | 原语 |

### 要变的部分

| 组件 | 当前 | 目标 | 变化 |
|------|------|------|------|
| `AgentBuilder::build()` | → `ToolUseLoop` | → `Graph<AgentState>` | **核心变更** |
| `ToolUseLoop` | 独立的运行时 | **薄 Facade**，持有 `Graph<AgentState>` | 保留，提供 invoke() 等高级 API |
| `AgentFlowNode` | 公开 struct | **直接删除** | 不再需要，无过渡期 |
| `build_react_graph()` | `pub(crate)` | 保持 `pub(crate)` | **不公开**，Runtime 实现细节 |
| `GraphFactory<S>` | 不存在 | **新增 trait** | 统一所有 DSL Builder 的接口 |

## GraphFactory Trait

```rust
/// Graph Factory — 所有 DSL Builder 的统一接口。
///
/// 类比：Axum 的 Router 构建器最终返回 Router；
/// 我们的各种 Builder 最终返回 Graph<S>。
pub trait GraphFactory<S> {
    fn build(self) -> Graph<S>;
}

// AgentBuilder (ReAct DSL)
impl GraphFactory<AgentState> for AgentBuilder { ... }

// 未来：
// impl GraphFactory<PlannerState> for PlannerBuilder { ... }
// impl GraphFactory<SupervisorState> for SupervisorBuilder { ... }
```

这是整个生态的统一抽象。所有 DSL Builder 都实现 `GraphFactory`，返回各种 `Graph<S>`。

## 迁移计划

### Phase 1：AgentBuilder::build() → Graph<AgentState>

**核心变更：** `AgentBuilder::build()` 的返回类型从 `ToolUseLoop` 变为 `Graph<AgentState, AgentStateMerge>`。

```rust
// Before
let loop_: ToolUseLoop = AgentBuilder::new(model)
    .system("...").tools([...]).build();

// After — 拿到标准 Graph
let graph: Graph<AgentState, AgentStateMerge> = AgentBuilder::new(model)
    .system("...").tools([...]).build();

// 直接用 ExecutionEngine 跑
let mut engine = ExecutionContext::<AgentState>::new(
    AgentState::from_messages(vec![Message::user_text("3+4*2")]),
    None, CancellationToken::new()
);
graph.run_inline(&mut engine, 100).await?;
```

`AgentBuilder` 内部逻辑不变：
- 仍然收集 tools、config、model
- `build()` 时调用 `build_react_graph()` 生成 `Graph<AgentState>`
- 实现 `GraphFactory<AgentState>` trait

**影响面：**
- `lellm-agent/src/runtime/builder.rs` — 核心修改
- `lellm-agent/src/lib.rs` — re-export 调整
- 所有 examples — 调用方式改变

### Phase 2：ToolUseLoop 重构为薄 Facade

**不删除** `ToolUseLoop`，而是重构为持有 `Graph<AgentState>` 的便捷层：

```rust
/// 薄 Facade — 持有 Graph，提供高级执行 API。
///
/// 不是独立的运行时，只是 Graph 的便捷包装。
pub struct ToolUseLoop {
    graph: Graph<AgentState, AgentStateMerge>,
    config: ToolUseConfig,  // 用于构建 ExecutionContext 的默认参数
}

impl ToolUseLoop {
    /// 从 Graph 创建 Facade
    pub fn new(graph: Graph<AgentState, AgentStateMerge>) -> Self { ... }

    /// 便捷执行 — 内部封装了 State 初始化 + 执行 + 结果提取
    pub async fn invoke(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> { ... }

    /// 流式执行
    pub async fn invoke_stream(&self, messages: Vec<Message>) -> AgentStream { ... }

    /// 结构化输出（未来）
    // pub async fn invoke_json<T>(&self, ...) -> Result<T, LlmError> { ... }
    // pub async fn invoke_structured<T>(&self, ...) -> Result<T, LlmError> { ... }
    // pub async fn invoke_until<F>(&self, predicate: F) -> Result<ToolUseResult, LlmError> { ... }
}

// AgentBuilder 仍然提供 .build_loop() 作为便捷入口
impl AgentBuilder {
    pub fn build(self) -> Graph<AgentState, AgentStateMerge> { ... }
    pub fn build_loop(self) -> ToolUseLoop {
        ToolUseLoop::new(self.build())
    }
}
```

**关键区别：**
- ToolUseLoop **不再** 内部构建 graph（那是 AgentBuilder 的事）
- ToolUseLoop **不再** 启动独立的 ExecutionEngine
- ToolUseLoop.invoke() 内部仍然调用 `graph.run_inline()`，但这是**同一个 ExecutionEngine**

**影响面：**
- `lellm-agent/src/runtime/runtime.rs` — 重构 ToolUseLoop
- 所有直接调用 `ToolUseLoop::execute()` 的地方 → `invoke()`

### Phase 3：直接删除 AgentFlowNode

`AgentFlowNode` 存在的唯一原因是把 `ToolUseLoop` 包装成 `FlowNode`，塞进外层的 `GraphBuilder`。

一旦 Agent = Graph，就不再需要这个包装层。

**不 deprecate，直接删除。** 理由：
- 它是历史包袱，没有任何独特的设计价值
- 它的功能被 `GraphBuilder::merge()` 替代
- 过渡期只会增加维护成本

**影响面：**
- `lellm-agent/src/runtime/flow_node.rs` — **删除整个文件**
- `lellm-agent/examples/calculator_graph.rs` — 重写
- `lellm-agent/examples/calculator_graph_mock.rs` — 重写

### Phase 4：GraphBuilder::merge() — Builder 阶段 AST 内联

**核心语义：** merge 发生在 **Builder 阶段**，不是 Graph 阶段。

```rust
// Builder 阶段完成 AST 合并
let mut builder = GraphBuilder::<AgentState, _>::new("workflow");

builder.node("init", init_node);

// merge ReAct graph 的 AST 到 builder
// prefix 参数避免节点名冲突
builder.merge(
    AgentBuilder::new(model).tools([...]).build(),
    "agent"  // prefix
);

builder.node("summary", summary_node);

// 手动连接入口/出口
builder.edge("init", "agent_start");      // 连接到 merge 的入口
builder.edge("agent_end", "summary");     // 从 merge 的出口连出

// compile → immutable Graph (NodeId 只分配一次)
let graph = builder.build()?;
```

**merge 的行为：**

```
Parent Builder:
  A → B → C

Merge ReAct Graph (prefix="agent"):
  START → budget_check → llm → post_llm_check → tool → budget_check (循环)
                                           → end

Result (扁平 AST):
  A → B → C → agent_budget_check → agent_llm → agent_post_llm_check → agent_tool → agent_budget_check (循环)
                                                    → agent_end → summary

  START → A
  END = summary
```

**merge 的规则：**
1. **NodeId 重写** — 所有节点名加 prefix + `_`
2. **Edge 重写** — 边的 from/to 相应调整
3. **START 删除** — 原 graph 的 start node 变为 merge 后的入口节点（以 `{prefix}_start` 命名）
4. **END 删除** — 原 graph 的 end node 变为 merge 后的出口节点（以 `{prefix}_end` 命名）
5. **最终生成一个真正的扁平图** — 没有 SubgraphNode，没有嵌套

**为什么必须在 Builder 阶段：**

```
Builder 阶段 merge 的好处：
  ✓ NodeId 只分配一次
  ✓ BarrierId 不会变化
  ✓ Checkpoint 不需要重新映射
  ✓ Trace 的节点编号保持稳定
  ✓ compile() 后 Graph 不可变

Graph 阶段 merge 的问题：
  ✗ 已编译的 graph 需要重新分配 NodeId
  ✗ Checkpoint 映射可能失效
  ✗ 多次 merge 导致不断重写
```

**API 设计：**

```rust
impl<S: WorkflowState, M: MergeStrategy<S>> GraphBuilder<S, M> {
    /// 将另一个 Graph 的 AST 内联到当前 Builder。
    ///
    /// 所有节点名加 prefix，边相应调整。
    /// 原 graph 的 START/END 被移除，入口/出口节点以 {prefix}_start / {prefix}_end 暴露。
    ///
    /// # 返回
    /// MergeEntry 包含入口和出口节点名，用于连接外部边。
    pub fn merge(
        &mut self,
        other: Graph<S, M>,
        prefix: impl Into<String>,
    ) -> MergeEntry {
        let prefix = prefix.into();
        for (name, kind) in other.nodes {
            self.nodes.insert(format!("{prefix}_{name}"), kind);
        }
        for edge in other.edges {
            self.edges.push(Edge {
                from: format!("{prefix}_{}", edge.from),
                to: format!("{prefix}_{}", edge.to),
                condition: edge.condition,
                fallback: edge.fallback,
                ..edge
            });
        }
        MergeEntry {
            entry: format!("{prefix}_{}", other.start),
            exit: format!("{prefix}_{}", other.end),
        }
    }
}

/// Merge 的结果，用于连接外部边。
pub struct MergeEntry {
    /// 入口节点名（原 graph 的 start node）
    pub entry: String,
    /// 出口节点名（原 graph 的 end node）
    pub exit: String,
}
```

**影响面：**
- `lellm-graph/src/graph.rs` — 新增 `GraphBuilder::merge()`
- 可能需要 `lellm-graph/src/graph_merge.rs` — 如果逻辑复杂

## 关键设计决策

### D1：build_react_graph() 不公开

**原因：** `build_react_graph()` 生成的图结构是 ReAct Runtime 的实现细节：

```
budget_check → llm → post_llm_check → tool → budget_check
```

如果公开，用户会开始依赖这个内部结构，插入自定义节点：

```
budget_check → llm → MyNode → post_llm_check → tool
```

一旦我们调整内部结构（加 Memory、改 Compactor、调整 Budget），用户的 graph 就坏了。

**类比：** LangGraph 的 `create_react_agent()` 也是黑盒，用户不能修改内部节点。

**真正应该公开的是 `AgentBuilder`（DSL），不是 `build_react_graph()`（Runtime 实现）。**

### D2：GraphBuilder::merge() — Builder 阶段 AST 内联

merge 发生在 Builder 阶段，不是 Graph 阶段。

理由见 Phase 4。

**不做 Subgraph Node（运行时压栈）：**
- 违背"唯一 Runtime"原则
- ExecutionEngine 需要支持 frame stack
- 一期不做

### D3：AgentBuilder 命名

**保持 `AgentBuilder` 名字，不重命名为 `ReactBuilder`。**

理由：
- "Agent Builder" 是用户最自然的搜索词
- `GraphFactory<AgentState>` trait 已经明确了它是 Graph Factory 的本质
- 重命名带来 breaking change，收益不大
- 未来 `PlannerBuilder`、`SupervisorBuilder` 是不同类型的 Builder，命名不会冲突

### D4：AgentState 的归属

`AgentState` 留在 `lellm-agent` crate。

理由：
- AgentState 是 agent 领域的概念
- `lellm-agent` 依赖 `lellm-graph`，返回 `Graph<AgentState>` 完全合法
- 迁移到 graph 或 core 是 premature abstraction

### D5：ToolUseLoop 保留为 Facade

**保留原因：** 提供高级 API，而不强迫用户写 boilerplate：

```rust
// 不用 ToolUseLoop (boilerplate)
let graph = AgentBuilder::new(model).tools([...]).build();
let state = AgentState::from_messages(messages);
let mut engine = ExecutionContext::new(state, None, CancellationToken::new());
graph.run_inline(&mut engine, 100).await?;
let result = extract_result(engine.state());

// 使用 ToolUseLoop (便捷)
let loop_ = AgentBuilder::new(model).tools([...]).build_loop();
let result = loop_.invoke(messages).await?;
```

**关键约束：** ToolUseLoop 内部**只能**调用 `Graph::run_inline()` / `Graph::run_stream()`，不能有自己的执行循环。

## 最终 API 全景

```rust
// ─── 层级 1：Graph 原语（lellm-graph）────

let mut builder = GraphBuilder::<MyState, _>::new("workflow");
builder.node("a", node_a);
builder.node("b", node_b);
builder.edge("a", "b", condition);
builder.end("b");
let graph: Graph<MyState> = builder.build()?;

// ─── 层级 2：DSL / Graph Factory（lellm-agent）────

// AgentBuilder — ReAct 模板
let graph: Graph<AgentState> = AgentBuilder::new(model)
    .system("你是一个助手")
    .tools([add, multiply])
    .max_iterations(10)
    .build();

// ─── 层级 3：便捷 Facade（lellm-agent）────

// ToolUseLoop — 高级 API 包装
let loop_ = AgentBuilder::new(model)
    .tools([...])
    .build_loop();

let result = loop_.invoke(messages).await?;
let stream = loop_.invoke_stream(messages).await;

// ─── 层级 4：组合（merge）────

let mut builder = GraphBuilder::<AgentState, _>::new("workflow");
builder.node("preprocess", preprocess_node);

let entry = builder.merge(
    AgentBuilder::new(model).tools([...]).build(),
    "agent"
);

builder.node("postprocess", postprocess_node);
builder.edge("preprocess", entry.entry);
builder.edge(entry.exit, "postprocess");
builder.end("postprocess");

let graph = builder.build()?;
// 只有一个 ExecutionEngine 跑整个图
```

## 文件变动清单

### 删除
| 文件 | 原因 |
|------|------|
| `lellm-agent/src/runtime/flow_node.rs` | AgentFlowNode 直接删除，无过渡期 |

### 核心修改
| 文件 | 改动 |
|------|------|
| `lellm-agent/src/runtime/builder.rs` | `build()` → `Graph<AgentState>`；新增 `build_loop()` → `ToolUseLoop`；实现 `GraphFactory<AgentState>` |
| `lellm-agent/src/runtime/runtime.rs` | `ToolUseLoop` 重构为薄 Facade，持有 Graph |
| `lellm-agent/src/runtime/mod.rs` | 模块导出调整 |
| `lellm-agent/src/lib.rs` | 公开 API 调整，导出 `GraphFactory` |
| `lellm-graph/src/graph.rs` | 新增 `GraphBuilder::merge()` |

### 新增
| 文件 | 内容 |
|------|------|
| `lellm-agent/src/factory.rs` | `GraphFactory<S>` trait 定义 |

### 示例重写
| 文件 | 改动 |
|------|------|
| `lellm-agent/examples/calculator_graph.rs` | 使用 AgentBuilder → Graph |
| `lellm-agent/examples/calculator_graph_mock.rs` | 使用 AgentBuilder → Graph |
| `lellm-agent/examples/simple_agent.rs` | 使用 build_loop().invoke() |
| `lellm-agent/examples/streaming_agent.rs` | 使用 build_loop().invoke_stream() |
| `lellm-agent/examples/tool_definition.rs` | 使用 AgentBuilder → Graph |
| `lellm-agent/examples/tool_use.rs` | 使用 AgentBuilder → Graph |
| `lellm-agent/examples/system_prompt.rs` | 使用 AgentBuilder → Graph |

## 不做的事情

1. **不公开 `build_react_graph()`** — Runtime 实现细节
2. **不做 Subgraph Node（运行时压栈）** — 一期只做 Builder 阶段 AST Merge
3. **不重命名 AgentBuilder** — 保持名字，用 GraphFactory trait 明确本质
4. **不动 GraphBuilder 核心逻辑** — 它是原语，只加 merge()
5. **不在 ToolUseLoop 中引入新执行循环** — 严格约束为 Graph 的 Facade

## 与 LangGraph 的对比

| 维度 | LangGraph | LeLLM (当前) | LeLLM (目标) |
|------|-----------|-------------|-------------|
| 唯一 Runtime | StateGraph + compile | ExecutionEngine + ToolUseLoop | ExecutionEngine |
| Agent 定义 | create_react_agent() (黑盒) | AgentBuilder → ToolUseLoop | AgentBuilder → Graph |
| 便捷执行 | graph.invoke() | ToolUseLoop.execute() | ToolUseLoop.invoke() (薄 Facade) |
| 自定义 Agent | StateGraph 手写 | GraphBuilder + AgentFlowNode | GraphBuilder 手写 / merge |
| 组合 | 子图节点 | AgentFlowNode (双层引擎) | GraphBuilder::merge() (单层引擎) |
| Checkpoint | 单层 | 双层（如果用了 AgentFlowNode） | 单层 |
| Graph Factory 抽象 | 无 | 无 | `GraphFactory<S>` trait |

## 收益排序

1. **AgentBuilder::build() → Graph** — 消除双重 Runtime，架构统一（最大收益）
2. **统一 ExecutionEngine** — Checkpoint/Trace/Cancellation/Streaming 全部单层
3. **GraphBuilder::merge() 编译期 AST 内联** — NodeId 稳定，Checkpoint 不需重映射
4. **删除 AgentFlowNode** — 减少代码量，消除反模式
5. **保留 AgentBuilder** — 保持 DSL 价值，build_react_graph() 保持私有

## 时间线

预估 2-3 天的工作量：

- **Day 1**：Phase 1 + Phase 2（AgentBuilder 返回 Graph，ToolUseLoop 重构为 Facade，GraphFactory trait）
- **Day 2**：Phase 3 + Phase 4（删除 AgentFlowNode，实现 GraphBuilder::merge()）
- **Day 3**：示例重写 + 文档更新
