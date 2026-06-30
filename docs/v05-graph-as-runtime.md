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

### D6：Subgraph 组合 — 借用 State + 递归执行

**核心决策：Subgraph 是 Graph AST 的一种节点；ExecutionEngine 对该节点采用递归执行语义。**

**状态所有权模型：**

ExecutionEngine **借用** State（`&'a mut S`），不拥有它。调用方持有 State 的所有权，
Engine 只在执行期间借用。这使得 Subgraph 组合成为可能：

```text
调用方
  ├── state: S                （拥有所有权）
  ├── engine: Engine<'_, S>   （借用 &mut state）
  │     └── SubgraphNode::execute()
  │           ├── lens.get(state) → &mut Inner
  │           ├── inner_engine: Engine<'_, Inner>  （借用 &mut inner）
  │           └── graph.run_inline(&mut inner_engine)
  └── state 仍然可用（engine drop 后借用释放）
```

**为什么借用 State：**

✅ **Engine 借用 State（已实现）**
```rust
pub struct ExecutionEngine<'a, S: WorkflowState> {
    state: &'a mut S,
    mutations: Vec<S::Mutation>,
    // ...
}
```

优点：
- 调用方持有 State 所有权，Engine 只在执行期间借用
- Subgraph 通过 StateLens 投影出 `&mut Inner`，创建内层 Engine
- 递归执行天然利用 Rust 的调用栈（不需要 FrameStack）
- borrow checker 在编译期保证状态安全

**Subgraph 执行流程：**

```rust
// SubgraphNode::execute()
pub async fn execute(
    &self,
    outer: &mut Outer,
    stream: Option<Arc<dyn StreamSink>>,
    cancel: CancellationToken,
) -> Result<(), GraphError> {
    // 1. 通过 Lens 投影出内层 State
    let inner_ref = self.lens.get(outer);

    // 2. 创建内层 Engine（借用 inner_ref）
    let mut inner_engine = ExecutionEngine::new(inner_ref, stream, cancel);

    // 3. 执行内层 Graph
    self.graph.run_inline(&mut inner_engine, self.max_steps).await?;

    // 4. inner_engine drop → 借用释放 → outer 可继续使用
    Ok(())
}
```

**架构优势：**

1. **递归执行 = 天然 Frame**
   - Rust 调用栈就是 FrameStack
   - 不需要额外的数据结构
   - 每层 Subgraph 是一个不可中断调用

2. **Checkpoint 一致性**
   - Checkpoint 通过 `state.snapshot()` 获取投影快照（P0-1）
   - 不依赖 Engine 持有 State 所有权

3. **并发支持**
   - Parallel 分支使用 `OwnedExecutionEngine<S>`（拥有 State 副本）
   - 每个分支独立执行，通过 MergeStrategy 合并

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

**FrameStack 归属修正：**

> **重要**：FrameStack **不在** ExecutionEngine 中。Engine 生命周期短（一次执行），
> FrameStack 生命周期长（整个恢复过程）。职责分离更清晰。

```rust
// ExecutionEngine — 一次执行，借用 State
pub struct ExecutionEngine<'a, S: WorkflowState> {
    state: &'a mut S,
    mutations: Vec<S::Mutation>,
    // ... 无 frame_stack
}

// ExecutionSession — 持有 FrameStack，管理恢复
pub struct ExecutionSession<S: WorkflowState> {
    state: S,
    frame_stack: FrameStack<S::Checkpoint>,  // 使用 Checkpoint 投影
    graph: Graph<S>,
}

impl<S: WorkflowState> ExecutionSession<S> {
    /// 创建 checkpoint — 保存当前执行位置 + 状态投影
    pub fn checkpoint(&self) -> SessionCheckpoint<S> {
        SessionCheckpoint {
            state: self.state.snapshot(),  // P0-1: 使用 snapshot()
            frames: self.frame_stack.clone(),
            graph_hash: self.graph.canonical_hash,  // P0-2: 使用 canonical hash
        }
    }

    /// 从 checkpoint 恢复
    pub fn restore(checkpoint: SessionCheckpoint<S>, graph: Graph<S>) -> Self {
        let state = S::restore(checkpoint.state);  // P0-1: 使用 restore()
        Self {
            state,
            frame_stack: checkpoint.frames,
            graph,
        }
    }
}
```

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
| `lellm-graph/src/subgraph_node.rs` | `SubgraphNode` 实现 — 运行时递归执行 | ✅ |
| `lellm-graph/src/state_lens.rs` | `StateLens` trait — 状态投影，不是状态转换 | ✅ |

### 新增（Phase 5：Compiler Inline Pass，可选优化）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/compiler/mod.rs` | Compiler 模块入口 | ✅ |
| `lellm-graph/src/compiler/inline_pass.rs` | Inline Pass — 骨架实现 | ✅ |

### 新增（Phase 7：P0-1 Checkpoint Projection）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/workflow_state.rs` | 添加 `type Checkpoint` + `snapshot()` + `restore()` | ⏸️ |
| `lellm-graph/src/checkpoint.rs` | `Checkpoint<S>` 使用 `S::Checkpoint` | ⏸️ |
| `lellm-agent/src/runtime/typed_state.rs` | `AgentCheckpoint` 定义 + 实现 | ⏸️ |

### 新增（Phase 8：P0-2 Graph Hash）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-agent/src/runtime/builder.rs` | `canonical_hash()` 方法 | ⏸️ |
| `lellm-graph/src/graph.rs` | `Graph` 携带 `canonical_hash` 字段 | ⏸️ |

### 新增（Phase 9：ExecutionSession）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/session.rs` | `ExecutionSession` — 持有 FrameStack | ⏸️ |
| `lellm-graph/src/session.rs` | `SessionCheckpoint` — 完整恢复快照 | ⏸️ |

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
struct Frame<S: WorkflowState> {
    graph_id: String,
    node_id: String,
    state: S::Checkpoint,  // P0-1: 使用 Checkpoint 关联类型
    cursor: usize,
}

struct FrameStack<S: WorkflowState> {
    frames: Vec<Frame<S>>,
}
```

**Checkpoint 时机：自动 frame boundary checkpoint**

- Node exit
- Subgraph exit
- Yield boundary（stream pause / barrier / tool boundary）

**恢复粒度：永远恢复 Whole WorkflowState，但 replay frame**

```rust
// 恢复流程
fn restore(checkpoint: SessionCheckpoint<S>, graph: Graph<S>) -> ExecutionSession<S> {
    // 1. 从 checkpoint snapshot 恢复 State（P0-1）
    let state = S::restore(checkpoint.state);

    // 2. restore FrameStack
    let frames = checkpoint.frames;

    // 3. 创建 session，从最后一个 Frame 恢复执行
    ExecutionSession { state, frame_stack: frames, graph }
}
```

**序列化约束：Checkpoint State ≠ Runtime State（P0-1 强制）**

```rust
// Runtime State（可能包含不可序列化字段）
struct AgentState {
    messages: Vec<Message>,
    iterations: usize,
    output_tokens: usize,
    cache: Arc<dyn ToolCatalog>,  // ❌ 不可序列化
    sender: mpsc::Sender<Event>,  // ❌ 不可序列化
}

// Checkpoint（只包含可序列化字段）
#[derive(Serialize, Deserialize)]
struct AgentCheckpoint {
    messages: Vec<Message>,
    iterations: usize,
    output_tokens: usize,
    // 不包含 cache, sender 等
}

// WorkflowState trait 强制实现（P0-1）
impl WorkflowState for AgentState {
    type Checkpoint = AgentCheckpoint;
    type Mutation = AgentMutation;

    fn snapshot(&self) -> AgentCheckpoint {
        AgentCheckpoint {
            messages: self.messages.clone(),
            iterations: self.iterations,
            output_tokens: self.output_tokens,
        }
    }

    fn restore(checkpoint: AgentCheckpoint) -> Self {
        AgentState {
            messages: checkpoint.messages,
            iterations: checkpoint.iterations,
            output_tokens: checkpoint.output_tokens,
            cache: Arc::new(ToolCatalog::default()),  // 重建
            sender: create_channel(),  // 重建
        }
    }
}
```

**Graph Hash 稳定性（P0-2）：**

```rust
// ❌ 当前：来自 compiled graph（HashMap 顺序不确定）
graph_hash = hash(nodes.keys())  // 每次 build 可能不同

// ✅ 目标：来自 DSL canonical form（顺序无关）
graph_hash = canonical_hash(model, sorted_tools, system_prompt)  // 永远相同
```

**最终架构：**

```rust
// 1. WorkflowState trait（P0-1 强制）
trait WorkflowState {
    type Checkpoint: Serialize + DeserializeOwned;
    type Mutation: StateMutation<Self>;
    fn snapshot(&self) -> Self::Checkpoint;
    fn restore(checkpoint: Self::Checkpoint) -> Self;
}

// 2. StateLens
trait StateLens<Outer, Inner> {
    fn get<'a>(&self, outer: &'a mut Outer) -> &'a mut Inner;
}

// 3. FrameStack（在 ExecutionSession 中，不在 Engine 中）
struct ExecutionSession<S: WorkflowState> {
    state: S,
    frame_stack: FrameStack<S>,
    graph: Graph<S>,
}

// 4. Checkpoint = Session snapshot
struct SessionCheckpoint<S: WorkflowState> {
    state: S::Checkpoint,  // P0-1: 投影，不是 raw state
    frames: FrameStack<S>,
    graph_hash: u64,  // P0-2: canonical hash
}

// 5. Checkpoint 时机
// 自动 frame boundary checkpoint
// 不是 node，不是 subgraph
// 是 frame transition
```

**一句话总结：**

checkpoint 不是保存 state，而是保存 execution position + state projection（通过 `type Checkpoint` 关联类型强制）。

## P0 设计补丁

### P0-1: Checkpoint Projection — `type Checkpoint` 关联类型

**核心问题**：文档说 `checkpoint = projection, not raw state`，但代码还是 `Checkpoint<S> { state: S }`。

**当前状态**：

```rust
// workflow_state.rs — 当前
pub trait WorkflowState: Clone + Send + Sync + Serialize + DeserializeOwned {
    type Mutation: StateMutation<Self>;
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = Self::Mutation>);
}

// checkpoint.rs — 当前
pub struct Checkpoint<S> {
    pub state: S,  // 直接持有 S，无 projection
}
```

**问题**：如果 `S` 包含 `Arc<dyn ToolCatalog>` 或 `mpsc::Sender`，序列化会失败。

**目标设计**：

```rust
// workflow_state.rs — 目标
pub trait WorkflowState: Clone + Send + Sync {
    /// 可序列化的 Checkpoint 快照（projection，不是 raw state）。
    type Checkpoint: Serialize + DeserializeOwned + Clone + Send;
    /// 状态变更命令。
    type Mutation: StateMutation<Self>;

    /// 创建 checkpoint 快照 — 只序列化必要字段。
    fn snapshot(&self) -> Self::Checkpoint;

    /// 从 checkpoint 恢复运行时状态。
    fn restore(checkpoint: Self::Checkpoint) -> Self;

    /// 批量应用 Mutation。
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = Self::Mutation>);
}

// checkpoint.rs — 目标
pub struct Checkpoint<S: WorkflowState> {
    pub checkpoint_id: CheckpointId,
    pub current_node: NodeId,
    pub state: S::Checkpoint,  // 使用 Checkpoint 关联类型，不是 S
    pub graph_hash: u64,
    pub created_at: std::time::SystemTime,
}
```

**示例：AgentState**：

```rust
// lellm-agent/src/runtime/typed_state.rs
pub struct AgentState {
    pub messages: Vec<Message>,
    pub iterations: usize,
    pub output_tokens: usize,
    pub stop_reason: Option<StopReason>,
    pub last_response: Option<ChatResponse>,
    pub total_tool_calls: usize,
    // ... 其他 runtime 字段
}

// 可序列化的 checkpoint 投影
#[derive(Serialize, Deserialize)]
pub struct AgentCheckpoint {
    pub messages: Vec<Message>,
    pub iterations: usize,
    pub output_tokens: usize,
    pub stop_reason: Option<StopReason>,
    pub total_tool_calls: usize,
    // 不包含: last_response（可重建）, Arc<dyn ...>, Sender 等
}

impl WorkflowState for AgentState {
    type Checkpoint = AgentCheckpoint;
    type Mutation = AgentMutation;

    fn snapshot(&self) -> AgentCheckpoint {
        AgentCheckpoint {
            messages: self.messages.clone(),
            iterations: self.iterations,
            output_tokens: self.output_tokens,
            stop_reason: self.stop_reason.clone(),
            total_tool_calls: self.total_tool_calls,
        }
    }

    fn restore(checkpoint: AgentCheckpoint) -> Self {
        AgentState {
            messages: checkpoint.messages,
            iterations: checkpoint.iterations,
            output_tokens: checkpoint.output_tokens,
            stop_reason: checkpoint.stop_reason,
            last_response: None,  // 重建时为空，下次 LLM 调用会填充
            total_tool_calls: checkpoint.total_tool_calls,
            // ... 其他 runtime 字段使用默认值
        }
    }

    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = Self::Mutation>) {
        for mutation in mutations {
            mutation.apply(self);
        }
    }
}
```

**收益**：

1. **Runtime State 可以包含不可序列化字段** — `Arc<dyn ...>`, `Sender`, `Cache` 等
2. **Checkpoint 是显式的 projection** — 开发者必须决定哪些字段需要持久化
3. **恢复时语义清晰** — `restore()` 明确哪些字段重建、哪些从 checkpoint 加载
4. **序列化安全** — 编译期保证 `Checkpoint` 可序列化

**实现步骤**：

1. 修改 `WorkflowState` trait，添加 `type Checkpoint` + `snapshot()` + `restore()`
2. 修改 `Checkpoint<S>` 使用 `S::Checkpoint`
3. 为 `AgentState` 实现 `Checkpoint` 投影
4. 为其他 State 类型（`State` 默认 HashMap）提供 fallback 实现

---

### P0-2: Graph Hash — 从 AST Canonical Form 计算

**核心问题**：当前 `graph_hash: u64` 来自 compiled graph 的节点顺序，但 `HashMap` 迭代顺序不确定。

**问题场景**：

```rust
// 第一次 build()
let graph1 = AgentBuilder::new(model).tools([a, b, c]).build();
// graph1.nodes: {"llm" → ..., "tool" → ..., "budget_check" → ...}
// hash1 = hash(llm, tool, budget_check)

// 第二次 build() — 相同输入
let graph2 = AgentBuilder::new(model).tools([a, b, c]).build();
// graph2.nodes: {"budget_check" → ..., "llm" → ..., "tool" → ...}  // HashMap 顺序不同
// hash2 = hash(budget_check, llm, tool)

// hash1 ≠ hash2 → checkpoint 失效！
```

**目标设计**：Graph Hash 来自 DSL 层的 canonical AST，不依赖 compiled graph 的节点顺序。

```rust
// lellm-agent/src/runtime/builder.rs
impl AgentBuilder {
    /// 计算 canonical AST hash — 不依赖 NodeId 顺序。
    ///
    /// Hash 输入：
    /// - model provider + model name
    /// - sorted tool names
    /// - system prompt hash
    /// - max_iterations
    /// - 其他影响 graph 结构的配置
    pub fn canonical_hash(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();

        // 1. Model (稳定)
        self.model.provider.hash(&mut hasher);
        self.model.name.hash(&mut hasher);

        // 2. Tools (排序后 hash，顺序无关)
        let mut tool_names: Vec<&str> = self.static_tools.iter()
            .map(|t| t.name.as_str())
            .collect();
        tool_names.sort();
        for name in &tool_names {
            name.hash(&mut hasher);
        }

        // 3. System prompt (hash 内容)
        if let Some(ref system) = self.config.system {
            // system.hash() 需要稳定实现
            format!("{:?}", system).hash(&mut hasher);
        }

        // 4. 结构性配置
        self.config.max_iterations.hash(&mut hasher);
        self.config.max_output_tokens.hash(&mut hasher);

        hasher.finish()
    }
}

// Graph 携带 canonical hash
pub struct Graph<S: WorkflowState, M: MergeStrategy<S>> {
    pub nodes: HashMap<String, NodeKind<S, M>>,
    pub edges: Vec<Edge>,
    pub canonical_hash: u64,  // 来自 DSL，不来自 nodes HashMap
    // ...
}

// Checkpoint 使用 canonical hash
impl Checkpoint {
    pub fn new(current_node: &str, state: S::Checkpoint, graph: &Graph<S, M>) -> Self {
        Self {
            checkpoint_id: CheckpointId::new(),
            current_node: NodeId(current_node.into()),
            state,
            graph_hash: graph.canonical_hash,  // 使用 canonical hash
            created_at: SystemTime::now(),
        }
    }
}
```

**收益**：

1. **Build 幂等** — 相同输入永远产生相同 hash
2. **Checkpoint 稳定** — 不因 HashMap 迭代顺序失效
3. **恢复安全** — graph_hash 不匹配时能正确拒绝

---

## 不做的事情

1. **不公开 `build_react_graph()`** — Runtime 实现细节
2. **不做 GraphFactory Trait** — 保持命名约定，不需要 trait
3. **不做 GraphBuilder::merge()** — Subgraph 作为原语，merge 作为 Compiler Pass
4. **不做 StateAdapter** — 使用 StateLens，零拷贝投影
5. **不做 Node/Subgraph 级别 checkpoint** — 使用 Frame Boundary Checkpoint
6. **不序列化 Runtime State** — 只序列化 Checkpoint State Projection（通过 `type Checkpoint` 关联类型强制）
7. **不重命名 AgentBuilder** — 保持名字
8. **不动 GraphBuilder 核心逻辑** — 它是原语
9. **不在 ToolUseLoop 中引入新执行循环** — 严格约束为 Graph 的 Facade
10. **不在 ExecutionEngine 中持有 FrameStack** — Engine 生命周期短，FrameStack 生命周期长（见 D6 修正）
11. **ExecutionSession 不持有 Stream** — Stream 属于 Engine，Session 只负责 state + frame_stack（见 Q3 修复）

## 待讨论设计点

### D10：ExecutionSession 是否需要 Arc\<Graph\>

**当前设计**：
```rust
pub struct ExecutionSession<S, M> {
    state: S,
    frame_stack: FrameStack<S>,
    graph: Graph<S, M>,  // 每个 Session 持有一份 Graph
}
```

**问题**：Graph 是 Immutable 的，每个 Session 都持有完整副本可能浪费内存。

**候选方案**：
```rust
pub struct ExecutionSession<S, M> {
    state: S,
    frame_stack: FrameStack<S>,
    graph: Arc<Graph<S, M>>,  // 共享 Graph
}

// 恢复时
impl SessionCheckpoint {
    fn restore(self, registry: &GraphRegistry) -> ExecutionSession {
        let graph = registry.get(self.graph_hash);
        ExecutionSession { state, frame_stack, graph }
    }
}
```

**收益**：1000 个 Session 共享同一个 Graph 实例。

**待决策**：是否在 v0.5 中实现，还是留到 v0.6。

### D11：canonical_hash 的 canonical 定义

**当前实现**：工具列表排序后 hash，工具顺序不影响 hash。

```rust
// 以下两种写法产生相同 hash
AgentBuilder::new(model).tools([a, b]).canonical_hash()
AgentBuilder::new(model).tools([b, a]).canonical_hash()  // 相同
```

**问题**：如果未来工具顺序影响 prompt（如 system prompt 中工具列表顺序），hash 需要包含顺序信息。

**候选定义**：
- **当前**：canonical = 排序后（工具顺序无关）
- **备选**：canonical = 插入顺序（工具顺序有关）

**待决策**：明确 canonical 的语义，避免未来 breaking change。

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
8. **Checkpoint Projection（P0-1）** — `type Checkpoint` 关联类型，强制 projection，序列化安全 ✅
9. **Graph Hash 稳定性（P0-2）** — 从 DSL canonical form 计算，不依赖 HashMap 顺序 ✅
10. **FrameStack 归属修正** — Engine 不持有 FrameStack，职责分离更清晰 ✅

## 实现状态

- [x] Phase 1：AgentBuilder::build() → Graph<AgentState>
- [x] Phase 2：ToolUseLoop 重构为薄 Facade
- [x] Phase 3：删除 AgentFlowNode
- [x] Phase 4：StateLens + SubgraphNode + SubgraphSpec
- [x] Phase 5：Compiler Inline Pass（骨架实现）
- [x] Phase 6：Checkpoint = Execution Frame Snapshot
- [x] Phase 7：P0-1 Checkpoint Projection — `type Checkpoint` 关联类型
- [x] Phase 8：P0-2 Graph Hash — canonical AST hash
- [x] Phase 9：ExecutionSession — FrameStack 归属修正

> Status: Implemented (v0.5)

## 时间线

已完成：Phase 1 ~ Phase 9 全部完成

v0.5 架构重构完成，P0 设计补丁已落地！

---

## 附录：grill-me 讨论记录

### 关键决策点

1. **GraphFactory Trait** → 去掉，保持命名约定
2. **ToolUseLoop** → 重构为持有预构建 Graph 的 Facade
3. **GraphBuilder::merge()** → 不实现，Subgraph 作为原语，merge 作为 Compiler Pass
4. **Subgraph 组合** → Subgraph 是 Graph AST 的一种节点；ExecutionEngine 借用 State（`&'a mut S`），通过 StateLens 投影执行内层 Graph
5. **StateLens vs StateAdapter** → 选择 StateLens，零拷贝投影
6. **Checkpoint** → 通过 `state.snapshot()` 获取投影快照，不依赖 Engine 持有所有权
7. **ExecutionContext 所有权** → Engine 借用 State（`&'a mut S`），调用方持有所有权。Parallel 分支使用 `OwnedExecutionEngine<S>`
8. **P0-1 Checkpoint Projection** → 引入 `type Checkpoint` 关联类型，强制 projection，序列化安全
9. **P0-2 Graph Hash** → 从 DSL canonical form 计算，不依赖 HashMap 顺序
10. **FrameStack 归属** → Engine 不持有 FrameStack，职责分离到 ExecutionSession

### 最终结论

- **两层世界划分**：DSL（稳定）和 Primitive（完全自由）
- **不做中间层**：避免 "半开放 ReAct Graph"
- **统一产物**：所有 Builder 都返回 Graph<S>
- **统一 Runtime**：只有一个 ExecutionEngine
- **零拷贝组合**：通过 StateLens 投影状态，不需要 clone/merge
- **Subgraph 是 Node**：在 Graph AST 中是 `NodeKind::Subgraph`；在 Runtime 中递归执行
- **Engine 借用 State**：`ExecutionEngine<'a, S>` 持有 `&'a mut S`，调用方持有所有权
- **Checkpoint = snapshot()**：通过 `type Checkpoint` 关联类型强制 projection
- **Graph Hash = canonical**：从 DSL 层计算，不依赖 compiled graph 的 HashMap 顺序
