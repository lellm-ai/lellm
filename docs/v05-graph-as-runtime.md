# v0.5 架构重构：Graph is Runtime, Agent is DSL

> 日期：2026-06-30
> 状态：设计方案 v5，已整合 grill-me 讨论反馈（核心决策变更：Subgraph 是 Engine 行为，不是 Node）

## 核心论点

**Graph 是唯一的 Runtime，Agent 是 Graph 的 DSL / 模板构建器。**

```
用户层
──────────────────────────────────────
AgentBuilder  PlannerBuilder  SupervisorBuilder
                    (DSL, build() → Graph<S>)

                    ↓ build() →
               Graph<AgentState>  Graph<PlannerState>  ...

                    ┌──────────────────────┐
                    │  ToolUseLoop (Facade) │  ← 薄层，持有 Graph + 高级 API
                    │  invoke()             │     invoke_stream()
                    │  invoke_stream()      │
                    └──────────────────────┘

                    ↓ 组合

               SubgraphSpec (Engine 行为，不是 Node)

──────────────────────────────────────
          Runtime (lellm-graph)
ExecutionEngine  Node  Edge  State  Graph  FrameStack

──────────────────────────────────────
          Primitive (lellm-graph)
GraphBuilder  (AST 构建器，compile() → immutable Graph)

──────────────────────────────────────
          Compiler (lellm-graph, 可选优化)
Inline Pass  (自动 merge Subgraph，用户不需要手动调用)
```

类比：`Router::new().route().layer().build()` 生成 Axum `Router`；
`AgentBuilder::new().tools().build()` 生成 `Graph<AgentState>`。

## 当前问题（已解决）

### 1. 双重 Runtime ✅

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

**问题：**
- **两个 ExecutionEngine** 在跑 —— Checkpoint、Trace、Cancellation、Streaming 全部双层
- **AgentFlowNode** 是 graph-in-graph 的反模式，递归执行
- **ToolUseLoop** 是一个独立的运行时实体，不是 graph 的一等公民

**解决方案：**
- AgentBuilder::build() 直接返回 Graph<AgentState>
- ToolUseLoop 重构为薄 Facade，持有预构建的 Graph
- 删除 AgentFlowNode

### 2. AgentBuilder 的黑盒性质 ✅

**问题：** `AgentBuilder::build()` → `ToolUseLoop`（一个不透明的执行循环），用户看不到 ReAct loop 的内部结构，也无法定制。

**解决方案：** AgentBuilder::build() 返回标准 Graph，用户可以直接用 `graph.run_inline()` 执行，或用 `build_loop().invoke()` 便捷执行。

### 3. 心智模型混乱 ✅

**问题：** 用户需要同时理解两套 API：`GraphBuilder`（"建图"）和 `AgentBuilder`（"建 Agent"）。

**解决方案：** 统一为两层世界：
- **DSL 层**：AgentBuilder、PlannerBuilder 等，稳定 API
- **Primitive 层**：GraphBuilder，完全自由

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
| `AgentBuilder::build()` | → `ToolUseLoop` | → `Graph<AgentState>` | **核心变更** ✅ |
| `ToolUseLoop` | 独立的运行时 | **薄 Facade**，持有 `Graph<AgentState>` | 保留，提供 invoke() 等高级 API ✅ |
| `AgentFlowNode` | 公开 struct | **直接删除** | 不再需要，无过渡期 ✅ |
| `build_react_graph()` | `pub(crate)` | 保持 `pub(crate)` | **不公开**，Runtime 实现细节 |
| `GraphFactory<S>` | 不存在 | **不实现** | 保持命名约定，不需要 trait |

### 两层世界划分

**世界一：Runtime DSL（稳定）**
- AgentBuilder
- PlannerBuilder
- SupervisorBuilder
- 特点：API 稳定，Runtime 可升级，内部结构不承诺

**世界二：Graph Primitive（完全自由）**
- GraphBuilder
- Node
- Edge
- Condition
- 特点：完全透明，想怎么改怎么改

**中间不存在第三层**
- 不做 "半开放 ReAct Graph"（既没有 DSL 稳定，又没有 Primitive 自由）

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

### D2：不做 GraphFactory Trait

**原因：** `GraphFactory<S>` trait 只有一个方法 `fn build(self) -> Graph<S>`，但各个 Builder 的配置参数（model、tools、system prompt 等）不同，`build()` 签名根本不同。

这个 trait 能提供的抽象价值是什么？
- 如果是为了统一类型签名，那 Rust 的 trait 默认不支持 trait object，泛型约束也几乎不会跨 Builder 使用
- 如果是为了文档/概念一致性，那一个空 trait 加注释就够了，不需要强制实现

**结论：** 更符合 Rust 风格的做法是统一 Builder 的约定，而不是统一它们的类型体系。真正需要统一的是最终产物 Graph<S> 和 Runtime，而不是所有 Builder 必须实现同一个 trait。

### D3：AgentBuilder 命名

**保持 `AgentBuilder` 名字，不重命名为 `ReactBuilder`。**

理由：
- "Agent Builder" 是用户最自然的搜索词
- 重命名带来 breaking change，收益不大
- 未来 `PlannerBuilder`、`SupervisorBuilder` 是不同类型的 Builder，命名不会冲突

**命名约定：** 所有 Builder 统一遵循：
- `::new(...)` — 创建构建器
- `.build()` → `Graph<_>` — 返回 Graph

而不是 `.compile()`、`.finish()`、`.create()`、`.generate()`。一致的命名本身就是最好的抽象。

### D4：AgentState 的归属

`AgentState` 留在 `lellm-agent` crate。

理由：
- AgentState 是 agent 领域的概念
- `lellm-agent` 依赖 `lellm-graph`，返回 `Graph<AgentState>` 完全合法
- 迁移到 graph 或 core 是 premature abstraction

### D5：ToolUseLoop 保留为薄 Facade

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

**结构设计：**

```rust
/// 薄 Facade — 持有预构建的 Graph，提供便捷执行 API。
pub struct ToolUseLoop {
    graph: Graph<AgentState, AgentStateMerge>,  // 预构建的 ReAct Graph
    config: ToolUseConfig,                       // 构建 ExecutionContext 的默认参数
}

impl ToolUseLoop {
    /// 从预构建的 Graph 创建 Facade。
    pub fn new(graph: Graph<AgentState, AgentStateMerge>, config: ToolUseConfig) -> Self;

    /// 便捷执行 — 内部调用 graph.run_inline()
    pub async fn invoke(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError>;

    /// 流式执行
    pub fn invoke_stream(&self, messages: Vec<Message>) -> AgentStream;
}

// AgentBuilder 仍然提供 .build_loop() 作为便捷入口
impl AgentBuilder {
    pub fn build(self) -> Graph<AgentState, AgentStateMerge> { ... }
    pub fn build_loop(self) -> ToolUseLoop {
        let config = self.config.clone();
        let graph = self.build();
        ToolUseLoop::new(graph, config)
    }
}
```

### D6：Subgraph 组合 — Engine 行为，不是 Node

**核心决策：Subgraph 不是 Node，而是 ExecutionEngine 的控制流概念。**

**为什么 Subgraph 不是 Node：**

❌ **take/put 模式**
```rust
let mut inner = lens.take(outer);
graph.run_inline(&mut ExecutionContext::new(inner));
lens.put(outer, inner);
```
问题：
- take() 期间，Outer State 处于"不完整"状态
- 如果 panic、cancel、timeout，put() 不一定执行
- Barrier、Checkpoint、Trace 都不知道现在 State 到底属于谁
- 后续如果支持并发节点，这种移动所有权会越来越复杂

❌ **ExecutionContext 改为引用语义**
```rust
ExecutionContext<'a, S> {
    state: &'a mut S,
}
```
问题：
- ExecutionContext 不只是 State，还持有 mutation buffer、current node、cancellation、checkpoint、stream、trace、barrier
- 整个 Engine 生命周期都会变成 `ExecutionContext<'a, S>`
- 几乎所有 API 都会变：`ExecutionEngine<'a, S>`
- 生命周期会迅速扩散，这是整个 Runtime 的基础设施修改
- 为了一个 Subgraph，不值得

✅ **Subgraph 是 Engine 行为**

```rust
// ExecutionEngine 内部处理
match node.kind() {
    Leaf => execute_leaf(),
    Flow => execute_flow(),
    Subgraph => execute_subgraph(),
}
```

**Subgraph 执行流程：**

```rust
// ExecutionEngine 内部
fn execute_subgraph(&mut self, subgraph: &SubgraphSpec) {
    // 1. 通过 Lens 投影状态
    let inner = subgraph.lens.project(&mut self.state);

    // 2. push Frame
    self.frame_stack.push(Frame {
        graph_id: subgraph.graph_id,
        node_id: current_node,
        state_snapshot: self.state.clone(),  // 或 checkpoint projection
    });

    // 3. 执行内层 Graph
    let mut inner_engine = ExecutionEngine::new(inner, self.stream.clone(), self.cancel.clone());
    inner_engine.run_inline(&subgraph.graph, max_steps).await;

    // 4. pop Frame
    self.frame_stack.pop();

    // 5. commit 状态变更
    subgraph.lens.commit(&mut self.state, inner_engine.into_state());
}
```

**架构优势：**

1. **FrameStack 天然支持**
   ```rust
   // Frame 0: Workflow
   // Frame 1: Agent
   // Frame 2: Planner

   // 恢复时
   Checkpoint {
       frames: [Frame0, Frame1, Frame2],
   }

   // ExecutionEngine 天然知道
   engine.resume(Frame2)
   ```

2. **Checkpoint 一致性**
   - Engine 知道当前 State 属于谁
   - Checkpoint 时机由 Engine 控制
   - 恢复时 Engine 重建 FrameStack

3. **并发支持**
   - 多个 Subgraph 可以并发执行
   - 每个 Subgraph 有独立的 ExecutionContext
   - 不需要移动所有权

**用户 API：**

```rust
// Builder 阶段
let agent_graph = AgentBuilder::new(model).tools([...]).build();

let mut builder = GraphBuilder::<WorkflowState, _>::new("workflow");
builder.node("init", init_node);
builder.subgraph("agent", agent_graph, AgentLens);  // 语法糖
builder.node("summary", summary_node);

builder.edge("init", "agent");
builder.edge("agent", "summary");

let graph = builder.build()?;
```

**编译后：**

```rust
// Builder AST
NodeKind::Subgraph(SubgraphNode { graph, lens })

// 编译后
CompiledNodeKind::Subgraph {
    graph_id: String,
    lens: Box<dyn StateLens<Outer, Inner>>,
}

// ExecutionEngine 根据 Kind 执行
match node.kind() {
    CompiledNodeKind::Subgraph { graph_id, lens } => {
        self.execute_subgraph(graph_id, lens).await;
    }
    // ...
}
```

**与 Checkpoint + FrameStack 的关系：**

```rust
// FrameStack 设计
struct Frame {
    graph_id: String,
    node_id: String,
    state_snapshot: WorkflowState,
    cursor: usize,
}

// ExecutionEngine 持有 FrameStack
struct ExecutionEngine<S> {
    state: S,
    frame_stack: Vec<Frame>,
    // ...
}

// Subgraph 执行时
fn execute_subgraph(&mut self, spec: &SubgraphSpec) {
    // push Frame
    self.frame_stack.push(Frame { ... });

    // 执行内层 Graph
    self.run_inner_graph(spec.graph).await;

    // pop Frame
    self.frame_stack.pop();
}

// Checkpoint 时
fn checkpoint(&self) -> Checkpoint {
    Checkpoint {
        state: self.state.checkpoint(),
        frames: self.frame_stack.clone(),
    }
}

// 恢复时
fn restore(&mut self, checkpoint: Checkpoint) {
    self.state = restore_state(checkpoint.state);
    self.frame_stack = checkpoint.frames;
    // 从最后一个 Frame 恢复执行
}
```

**为什么这是正确的设计：**

1. **Subgraph 是控制流概念** — 不是普通 Node，而是 Engine 的递归执行
2. **FrameStack 天然支持** — Engine 知道当前 State 属于谁
3. **Checkpoint 一致** — Engine 控制 Checkpoint 时机
4. **并发友好** — 每个 Subgraph 有独立的 ExecutionContext
5. **生命周期清晰** — 不需要修改 ExecutionContext 的所有权模型

**Subgraph 执行语义：**
- 运行时遇到 SubgraphNode 时，递归调用 `graph.run_inline()`
- 通过 StateLens 投影状态，不需要 Outer → Inner → Outer 复用
- 退出 Subgraph 后，借用结束，继续操作外层 State
- 不需要 Frame Stack，只是递归函数调用

**Compiler Inline Pass（可选优化）：**

```rust
// 用户不需要手动调用 merge
// Compiler 在 compile() 时自动决定是否内联

let graph = builder.build()?;  // 自动触发 Inline Pass

// 编译器内部流程：
// 1. 分析 SubgraphNode
// 2. 评估是否值得内联（基于图大小、调用频率等）
// 3. 如果值得：展开 Subgraph，合并到外层 Graph
// 4. 如果不值得：保持 Subgraph，运行时递归执行
```

**为什么不做 GraphBuilder::merge()：**

1. **封装破坏** — prefix 重命名导致外部依赖内部节点名
   ```rust
   // ❌ 不可接受
   builder.merge(agent, "agent");
   builder.edge("init", "agent_budget_check");  // 依赖内部节点名
   ```

2. **语义错误** — merge 是编译器优化，不是编程模型
   - 像 LLVM 的 function inlining
   - 用户不应该手动调用 `builder.merge()`
   - 应该由 `compile()` 自动决定

3. **Checkpoint 简化** — 不需要 merge 也能实现
   ```rust
   // Subgraph Checkpoint
   Workflow → SubgraphNode → Checkpoint {
       node = "agent",
       subgraph_checkpoint = ...
   }

   // 恢复时
   Workflow → 进入 agent → 恢复 agent checkpoint
   ```

4. **性能收益有限** — AST 内联的主要价值是编译器优化
   - 用户手动 merge 不会比自动 inline 更好
   - 编译器可以基于 profile 数据做更优决策

**未来 Compiler Pass 实现：**

```rust
// lellm-graph/src/compiler/inline_pass.rs

pub struct InlinePass;

impl CompilerPass for InlinePass {
    fn run(&self, graph: &mut Graph) {
        // 1. 识别所有 SubgraphNode
        // 2. 评估每个 Subgraph 是否值得内联
        //    - 图大小 < 阈值
        //    - 没有外部依赖
        //    - StateLens 是纯投影
        // 3. 如果值得：展开 Subgraph，合并节点和边
        // 4. 重映射 NodeId
        // 5. 优化 CFG（死代码消除、Barrier 合并等）
    }
}
```

## 最终 API 全景

```rust
// ─── 层级 1：Graph 原语（lellm-graph）────

let mut builder = GraphBuilder::<MyState, _>::new("workflow");
builder.node("a", node_a);
builder.node("b", node_b);
builder.edge("a", "b", condition);
builder.end("b");
let graph: Graph<MyState> = builder.build()?;

// ─── 层级 2：DSL（lellm-agent）────

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
let stream = loop_.invoke_stream(messages);

// ─── 层级 4：组合（Subgraph）────

let agent_graph = AgentBuilder::new(model).tools([...]).build();

let mut builder = GraphBuilder::<WorkflowState, _>::new("workflow");
builder.node("preprocess", preprocess_node);
builder.node("agent", NodeKind::Subgraph(SubgraphNode::new(agent_graph, AgentLens)));
builder.node("postprocess", postprocess_node);

builder.edge("preprocess", "agent");
builder.edge("agent", "postprocess");

let graph = builder.build()?;  // 自动触发 Inline Pass（可选）
// 只有一个 ExecutionEngine 跑整个图

// ─── 层级 5：Compiler Pass（可选优化）────

// 用户不需要手动调用 merge
// Compiler 在 compile() 时自动决定是否内联 Subgraph
// 基于图大小、调用频率等 profile 数据做决策
```

## 文件变动清单

### 删除
| 文件 | 原因 |
|------|------|
| `lellm-agent/src/runtime/flow_node.rs` | AgentFlowNode 直接删除，无过渡期 ✅ |

### 核心修改
| 文件 | 改动 | 状态 |
|------|------|------|
| `lellm-agent/src/runtime/builder.rs` | `build()` → `Graph<AgentState>`；新增 `build_loop()` → `ToolUseLoop` | ✅ |
| `lellm-agent/src/runtime/runtime.rs` | `ToolUseLoop` 重构为薄 Facade，持有 Graph | ✅ |
| `lellm-agent/src/runtime/mod.rs` | 模块导出调整 | ✅ |
| `lellm-agent/src/lib.rs` | 公开 API 调整 | ✅ |
| `lellm-agent/src/runtime/typed_state.rs` | `AgentStateMerge` 添加 Clone | ✅ |

### 新增（Phase 4：Subgraph 组合）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/subgraph_node.rs` | `SubgraphNode` 实现 — 运行时递归执行 | ⏸️ 待实现 |
| `lellm-graph/src/state_lens.rs` | `StateLens` trait — 状态投影，不是状态转换 | ⏸️ 待实现 |

### 新增（Phase 5：Compiler Inline Pass，可选优化）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/compiler/mod.rs` | Compiler 模块入口 | ⏸️ 待实现 |
| `lellm-graph/src/compiler/inline_pass.rs` | Inline Pass — 自动 merge Subgraph | ⏸️ 待实现 |

### 示例重写
| 文件 | 改动 | 状态 |
|------|------|------|
| `lellm-agent/examples/calculator_graph.rs` | 使用 AgentBuilder → Graph | ✅ |
| `lellm-agent/examples/calculator_graph_mock.rs` | 使用 AgentBuilder → Graph | ✅ |
| `lellm-agent/examples/simple_agent.rs` | 使用 build_loop().invoke() | ✅ |
| `lellm-agent/examples/streaming_agent.rs` | 使用 build_loop().invoke_stream() | ✅ |
| `lellm-agent/examples/tool_definition.rs` | 使用 AgentBuilder → Graph | ✅ |
| `lellm-agent/examples/tool_use.rs` | 使用 AgentBuilder → Graph | ✅ |
| `lellm-agent/examples/system_prompt.rs` | 使用 AgentBuilder → Graph | ✅ |

### D7：StateLens — 状态投影，不是状态转换

**核心决策：使用 StateLens，不是 StateAdapter。**

**为什么不做 StateAdapter：**

```rust
// ❌ StateAdapter — 状态转换（有 clone/merge）
trait StateAdapter<O, I> {
    fn extract(&self, outer: &O) -> I;
    fn merge(&self, inner: I, outer: &mut O);
}
```

问题：
- 需要 clone 和 merge，性能开销
- 两个闭包容易不一致（extract 忘了 merge）
- 生命周期复杂

**为什么选择 StateLens：**

```rust
// ✅ StateLens — 状态投影（零拷贝）
trait StateLens<O, I> {
    fn get<'a>(&self, outer: &'a mut O) -> &'a mut I;
}
```

优势：
- 零拷贝，只有借用
- Agent Graph 不知道 WorkflowState 存在
- 保持 AgentBuilder 返回 Graph<AgentState> 不变
- 符合 Rust 风格，生命周期清晰

**使用示例：**

```rust
// WorkflowState 包含 AgentState
struct WorkflowState {
    planner: PlannerState,
    agent: AgentState,
    memory: MemoryState,
}

// StateLens 实现
struct AgentLens;
impl StateLens<WorkflowState, AgentState> for AgentLens {
    fn get<'a>(&self, outer: &'a mut WorkflowState) -> &'a mut AgentState {
        &mut outer.agent
    }
}

// Subgraph 组合
let agent_graph = AgentBuilder::new(model).tools([...]).build();

let mut builder = GraphBuilder::<WorkflowState, _>::new("workflow");
builder.node(
    "agent",
    NodeKind::Subgraph(SubgraphNode::new(agent_graph, AgentLens)),
);

// Agent Graph 只操作 &mut AgentState
// 不知道 WorkflowState 存在
```

**借用边界设计：**

```rust
// ExecutionContext 负责运行时资源，不持有业务 State
struct ExecutionContext<S> {
    state: Option<S>,  // 业务 State（可选）
    stream: Option<Arc<dyn StreamSink>>,
    cancellation: CancellationToken,
    // ...
}

// GraphRunner 在进入 Subgraph 时
// 通过 StateLens 投影出 &mut AgentState
// 退出 Subgraph 后，借用结束
// 再继续操作 WorkflowState
```

**与 Adapter 的对比：**

| 维度 | StateAdapter | StateLens |
|------|-------------|-----------|
| 语义 | 状态转换（Conversion） | 状态投影（Projection） |
| 拷贝 | 需要 clone + merge | 零拷贝，只有借用 |
| 复杂度 | 两个闭包，容易不一致 | 一个方法，简单清晰 |
| 性能 | 有开销 | 无开销 |
| 封装 | Agent 知道 WorkflowState | Agent 不知道 WorkflowState |

**最终选择：StateLens**

```rust
// 用户 API
builder.node(
    "agent",
    NodeKind::Subgraph(
        SubgraphNode::new(agent_graph)
            .lens(AgentLens)
    ),
);

// 或者更简洁
builder.subgraph("agent", agent_graph, AgentLens);
```

### D8：Checkpoint = Execution Frame Snapshot

**核心洞察：checkpoint 不是保存 state，而是保存 execution position + state projection。**

**checkpoint 的边界单位是 Graph Execution Frame，不是 WorkflowState 或 Node。**

**三种直觉方案的问题：**

❌ **方案 1：每个 Node checkpoint**
- Subgraph 内部会有 10~100 steps（ReAct loop）
- 你会在 loop 中间崩溃
- checkpoint 粒度 ≠ 实际执行边界
- 恢复时语义不一致

❌ **方案 2：Subgraph entry/exit checkpoint**
- Subgraph 内部是 loop graph，不是 atomic unit
- entry/exit 无法表达 loop 中间状态
- tool 已执行但 checkpoint 还没写，或者 checkpoint 已写但 tool 还没执行

❌ **方案 3：用户显式 checkpoint**
- 用户不会知道 ReAct loop 内部什么时候该存
- 也不会知道 barrier / tool / llm 的隐含边界
- 会破坏框架价值

**正确模型：Graph Execution = Frame Stack**

```rust
struct Frame {
    graph_id: String,
    node_id: String,
    state_snapshot: WorkflowState,  // 或者 CheckpointState
    cursor: usize,  // 执行位置
}

struct FrameStack {
    frames: Vec<Frame>,
}
```

**Checkpoint 时机：自动 frame boundary checkpoint**

- Node exit
- Subgraph exit
- Yield boundary（stream pause / barrier / tool boundary）

**恢复粒度：永远恢复 Whole WorkflowState，但 replay frame**

```rust
// 恢复流程
fn restore(checkpoint: Checkpoint) {
    // 1. load WorkflowState
    let state = load_state(checkpoint.state);

    // 2. restore FrameStack
    let frames = restore_frames(checkpoint.frames);

    // 3. resume execution
    resume_execution(state, frames);
}
```

**序列化约束：Checkpoint State ≠ Runtime State**

```rust
// Runtime State（不可序列化）
struct WorkflowState {
    agent: AgentState,
    planner: PlannerState,
    cache: Arc<...>,  // ❌ 不可序列化
    channels: mpsc::...,  // ❌ 不可序列化
}

// Checkpoint State（可序列化）
struct CheckpointState {
    agent: AgentCheckpoint,
    planner: PlannerCheckpoint,
    memory: MemoryCheckpoint,
}

// 核心原则：checkpoint = projection, not raw state
impl WorkflowState {
    fn checkpoint(&self) -> CheckpointState {
        // 只序列化必要的字段
        CheckpointState {
            agent: self.agent.to_checkpoint(),
            planner: self.planner.to_checkpoint(),
            memory: self.memory.to_checkpoint(),
        }
    }
}
```

**最终架构：**

```rust
// 1. WorkflowState（runtime）
struct WorkflowState {
    agent: AgentState,
    planner: PlannerState,
    memory: MemoryState,
    // ... 其他 runtime 字段
}

// 2. StateLens
trait StateLens {
    fn project(&self, state: &WorkflowState) -> AgentStateView;
}

// 3. FrameStack
struct Frame {
    graph_id: String,
    node_id: String,
    state_snapshot: WorkflowState,
    cursor: usize,
}

// 4. Checkpoint = FrameStack snapshot
struct Checkpoint {
    state: CheckpointState,  // 可序列化的 state projection
    frames: Vec<Frame>,      // 执行位置
}

// 5. Checkpoint 时机
// 自动 frame boundary checkpoint
// 不是 node，不是 subgraph
// 是 frame transition
```

**一句话总结：**

checkpoint 不是保存 state，而是保存 execution position + state projection。

## 不做的事情

1. **不公开 `build_react_graph()`** — Runtime 实现细节
2. **不做 GraphFactory Trait** — 保持命名约定，不需要 trait
3. **不做 GraphBuilder::merge()** — Subgraph 作为原语，merge 作为 Compiler Pass
4. **不做 StateAdapter** — 使用 StateLens，零拷贝投影
5. **不做 Node/Subgraph 级别 checkpoint** — 使用 Frame Boundary Checkpoint
6. **不序列化 Runtime State** — 只序列化 Checkpoint State Projection
7. **不重命名 AgentBuilder** — 保持名字
8. **不动 GraphBuilder 核心逻辑** — 它是原语
9. **不在 ToolUseLoop 中引入新执行循环** — 严格约束为 Graph 的 Facade

## 与 LangGraph 的对比

| 维度 | LangGraph | LeLLM (v0.5) |
|------|-----------|-------------|
| 唯一 Runtime | StateGraph + compile | ExecutionEngine |
| Agent 定义 | create_react_agent() (黑盒) | AgentBuilder → Graph |
| 便捷执行 | graph.invoke() | ToolUseLoop.invoke() (薄 Facade) |
| 自定义 Agent | StateGraph 手写 | GraphBuilder 手写 / SubgraphNode |
| 组合 | 子图节点 | SubgraphNode (递归执行) |
| Checkpoint | 单层 | 单层 ✅ |
| Graph Factory 抽象 | 无 | 无（保持命名约定） |

## 收益

1. **AgentBuilder::build() → Graph** — 消除双重 Runtime，架构统一（最大收益）✅
2. **统一 ExecutionEngine** — Checkpoint/Trace/Cancellation/Streaming 全部单层 ✅
3. **ToolUseLoop 重构为薄 Facade** — 持有预构建 Graph，不再每次重新构建 ✅
4. **删除 AgentFlowNode** — 减少代码量，消除反模式 ✅
5. **保留 AgentBuilder** — 保持 DSL 价值，build_react_graph() 保持私有 ✅
6. **不做 GraphFactory Trait** — 符合 Rust 风格，统一命名约定 ⏸️
7. **不做 GraphBuilder::merge()** — Subgraph 作为原语，merge 作为 Compiler Pass ⏸️

## 实现状态

- [x] Phase 1：AgentBuilder::build() → Graph<AgentState>
- [x] Phase 2：ToolUseLoop 重构为薄 Facade
- [x] Phase 3：删除 AgentFlowNode
- [ ] Phase 4：Subgraph 作为 Engine 行为（SubgraphSpec + FrameStack）
- [ ] Phase 5：Compiler Inline Pass（可选优化）
- [ ] Phase 6：Checkpoint = Execution Frame Snapshot（待实现）

## 时间线

已完成：Phase 1 + Phase 2 + Phase 3

待实现：Phase 4 + Phase 5 + Phase 6

---

## 附录：grill-me 讨论记录

### 关键决策点

1. **GraphFactory Trait** → 去掉，保持命名约定
2. **ToolUseLoop** → 重构为持有预构建 Graph 的 Facade
3. **GraphBuilder::merge()** → 不实现，Subgraph 作为原语，merge 作为 Compiler Pass
4. **Subgraph 组合** → Engine 行为，不是 Node；由 ExecutionEngine 负责 Frame 管理、状态投影、Checkpoint 和恢复
5. **StateLens vs StateAdapter** → 选择 StateLens，零拷贝投影
6. **Checkpoint** → Execution Frame Snapshot，不是 State 问题，是 Execution Control Problem
7. **ExecutionContext 所有权** → 不要为了 Subgraph 改 ExecutionContext 的所有权模型

### 最终结论

- **两层世界划分**：DSL（稳定）和 Primitive（完全自由）
- **不做中间层**：避免 "半开放 ReAct Graph"
- **统一产物**：所有 Builder 都返回 Graph<S>
- **统一 Runtime**：只有一个 ExecutionEngine
- **零拷贝组合**：通过 StateLens 投影状态，不需要 clone/merge
- **Subgraph 是 Engine 行为**：不是普通 Node，而是 ExecutionEngine 的控制流概念
- **Checkpoint = Execution Position + State Projection**：不是保存 state，而是保存 execution position + state projection
