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

**解决方案：** AgentBuilder::build() 返回标准 Graph，用户可以直接用 `graph.run_inline()` 执行，或用 `compile().invoke()` 便捷执行。

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
| `build_react_graph()` | `pub(crate)` | 保持 `pub(crate)` | **不公开**，Runtime 实现细节；返回裸 `Graph`，由 `AgentBuilder::build()` 包装 `Arc` |
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
let graph = AgentBuilder::new(model).tools([...]).build();  // 返回 Arc<Graph>
let state = AgentState::from_messages(messages);
let mut engine = ExecutionEngine::new(&mut state, None, CancellationToken::new());
graph.run_inline(&mut engine, 100).await?;
let result = extract_result(&state);

// 使用 ToolUseLoop (便捷)
let loop_ = AgentBuilder::new(model).tools([...]).compile();
let result = loop_.invoke(messages).await?;
```

**关键约束：** ToolUseLoop 内部**只能**调用 `Graph::run_inline()`，不能有自己的执行循环。

**结构设计：**

```rust
/// 薄 Facade — 持有预构建的 Graph，提供便捷执行 API。
pub struct ToolUseLoop {
    graph: Arc<Graph<AgentState, AgentStateMerge>>,  // D10: Arc 共享
    config: ToolUseConfig,
}

impl ToolUseLoop {
    /// 从预构建的 Graph 创建 Facade。
    pub fn new(graph: Arc<Graph<AgentState, AgentStateMerge>>, config: ToolUseConfig) -> Self;

    /// 便捷执行 — 内部调用 graph.run_inline()
    pub async fn invoke(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError>;

    /// 流式执行
    pub fn invoke_stream(&self, messages: Vec<Message>) -> AgentStream;
}

// AgentBuilder 仍然提供 .compile() 作为便捷入口
impl AgentBuilder {
    pub fn build(self) -> Arc<Graph<AgentState, AgentStateMerge>> { ... }
    pub fn compile(self) -> ToolUseLoop {
        let config = self.config.clone();
        let graph = self.build();  // 返回 Arc<Graph>
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
  │     └── CompiledSubgraph::execute()
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
// CompiledSubgraph::execute() (通过 StateProjector trait)
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
builder.subgraph("agent", SubgraphSpec::new(agent_graph, AgentLens));  // 语法糖
builder.node("summary", summary_node);

builder.edge("init", "agent");
builder.edge("agent", "summary");

let graph = builder.build()?;
```

**编译后：**

```rust
// Builder 阶段（用户代码）
let spec = SubgraphSpec::new(agent_graph, AgentLens);
builder.subgraph("agent", spec);  // 语法糖

// 内部编译：SubgraphSpec → CompiledSubgraph
NodeKind::Subgraph(CompiledSubgraph {
    projector: Arc::new(spec),  // SubgraphSpec implements StateProjector
    max_steps: 1000,
})

// Engine 执行
match node.kind {
    NodeKind::Subgraph(subgraph) => {
        subgraph.execute(engine.state_mut(), stream, cancel).await;
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

let graph = builder.build()?;  // 不触发优化，仅验证 AST
// 或
let graph = builder.compile()?;  // 验证 + 运行 Compiler Pass（如 InlinePass）

// ⚠️ 当前 InlinePass 为骨架实现，仅识别 Subgraph 并收集统计信息，
// 不执行实际的内联展开。完整的内联逻辑待实现。
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
   Workflow → SubgraphSpec → Checkpoint {
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
        // 1. 识别所有 NodeKind::Subgraph 节点
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
builder.edge("a", "b");           // 无条件边
builder.edge_if("a", "b", cond); // 有条件边
builder.end("b");
let graph: Graph<MyState> = builder.build()?;  // 仅验证 AST

// ─── 层级 2：DSL（lellm-agent）────

// AgentBuilder — ReAct 模板
let graph: Arc<Graph<AgentState>> = AgentBuilder::new(model)
    .system("你是一个助手")
    .tools([add, multiply])
    .max_iterations(10)
    .build();

// ─── 层级 3：便捷 Facade（lellm-agent）────

// ToolUseLoop — 高级 API 包装
let loop_ = AgentBuilder::new(model)
    .tools([...])
    .compile();

let result = loop_.invoke(messages).await?;
let stream = loop_.invoke_stream(messages);

// ─── 层级 4：组合（Subgraph）────

let agent_graph = AgentBuilder::new(model).tools([...]).build();

let mut builder = GraphBuilder::<WorkflowState, _>::new("workflow");
builder.node("preprocess", preprocess_node);
builder.subgraph("agent", SubgraphSpec::new(agent_graph, AgentLens));
builder.node("postprocess", postprocess_node);

builder.edge("preprocess", "agent");
builder.edge("agent", "postprocess");

let graph = builder.build()?;  // 或 builder.compile()? 运行优化 pass
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
| `lellm-agent/src/runtime/builder.rs` | `build()` → `Graph<AgentState>`；新增 `compile()` → `ToolUseLoop` | ✅ |
| `lellm-agent/src/runtime/runtime.rs` | `ToolUseLoop` 重构为薄 Facade，持有 Graph | ✅ |
| `lellm-agent/src/runtime/mod.rs` | 模块导出调整 | ✅ |
| `lellm-agent/src/lib.rs` | 公开 API 调整 | ✅ |
| `lellm-agent/src/runtime/typed_state.rs` | `AgentStateMerge` 添加 Clone | ✅ |
| `lellm-graph/src/graph.rs` | `GraphBuilder` 添加 `canonical_hash()` + `compile()`；`build()` 自动计算结构 hash | ✅ |
| `lellm-graph/src/compiler/` | Compiler 模块 — `CompilerPass` trait + `InlinePass` 骨架 | ✅ |

### 新增（Phase 4：Subgraph 组合）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/subgraph_spec.rs` | `SubgraphSpec` — Builder 阶段强类型描述 | ✅ |
| `lellm-graph/src/compiled_subgraph.rs` | `CompiledSubgraph` + `StateProjector` trait — 类型擦除执行器 | ✅ |
| `lellm-graph/src/state_lens.rs` | `StateLens` trait — 状态投影，不是状态转换 | ✅ |

### 新增（Phase 5：Compiler Inline Pass，可选优化）
| 文件 | 内容 | 状态 |
|------|------|------|
| `lellm-graph/src/compiler/mod.rs` | Compiler 模块入口 | ✅ |
| `lellm-graph/src/compiler/inline_pass.rs` | Inline Pass — 骨架实现（仅识别 + 统计，不执行内联） | ⏸️ |

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
| `lellm-agent/examples/simple_agent.rs` | 使用 compile().invoke() | ✅ |
| `lellm-agent/examples/streaming_agent.rs` | 使用 compile().invoke_stream() | ✅ |
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
builder.subgraph("agent", SubgraphSpec::new(agent_graph, AgentLens));

// Agent Graph 只操作 &mut AgentState
// 不知道 WorkflowState 存在
```

**借用边界设计：**

```rust
// ExecutionEngine 借用 State，不拥有它（pub type ExecutionContext = ExecutionEngine）
pub struct ExecutionEngine<'a, S: WorkflowState> {
    state: &'a mut S,  // 借用，不是 Option<S>
    stream: Option<Arc<dyn StreamSink>>,
    cancel: CancellationToken,
    mutations: Vec<S::Mutation>,
    flow_events: Vec<FlowEvent>,
    // ...
}

// Engine 在进入 Subgraph 时
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
let spec = SubgraphSpec::new(agent_graph, AgentLens);
builder.subgraph("agent", spec);  // 语法糖

// 等价于：
// builder.node("agent", NodeKind::Subgraph(spec.compile()));
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
    frames: Vec<Frame<S>>,  // Frame<S> 内部使用 S::Checkpoint
}
```

**前置约束：** `S::Checkpoint: Debug` — `Frame` / `FrameStack` / `SessionCheckpoint` 需要序列化，
因此要求 `Checkpoint` 实现 `Debug`。如果用户的 Checkpoint 类型不含 Debug，需要包装一层。

**Checkpoint 时机：自动 frame boundary checkpoint**

- Node exit
- Subgraph exit
- Yield boundary（stream pause / barrier / tool boundary）

**恢复粒度：永远恢复 Whole WorkflowState，但 replay frame**

```rust
// 恢复流程
pub fn restore(
    checkpoint: SessionCheckpoint<S>,
    graph: Arc<Graph<S, M>>,
) -> Result<Self, SessionError> {
    // P0-2: 校验 graph_hash
    if checkpoint.graph_hash != graph.canonical_hash() {
        return Err(SessionError::GraphMismatch { ... });
    }
    // 1. 从 checkpoint snapshot 恢复 State（P0-1）
    let state = S::restore(checkpoint.state);

    // 2. restore FrameStack
    let frames = checkpoint.frames;

    // 3. 创建 session，从最后一个 Frame 恢复执行
    Ok(Self { state, frame_stack: frames, graph })
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
// ❌ 早期：来自 compiled graph（HashMap 顺序不确定）
graph_hash = hash(nodes.keys())  // 每次 build 可能不同
// ✅ 已修复：Graph.nodes 使用 IndexMap，但 DSL 层仍然计算 canonical_hash

// ✅ 目标：来自 DSL canonical form（不排序，保持输入顺序，见 D11）
graph_hash = canonical_hash(model, tools_in_insertion_order, system_prompt)
// .tools([A, B]) 和 .tools([B, A]) 产生不同 hash
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
//    S::Checkpoint: Debug 约束（Frame/FrameStack 序列化需要）
struct ExecutionSession<S: WorkflowState, M: MergeStrategy<S>>
where
    S::Checkpoint: Debug,
{
    state: S,
    frame_stack: FrameStack<S>,
    graph: Arc<Graph<S, M>>,  // D10: Arc 共享
}

// 4a. Checkpoint<S> — 单 Graph 级别检查点
//     用于 ExecutionEngine 内部，记录当前执行位置
struct Checkpoint<S: WorkflowState> {
    checkpoint_id: CheckpointId,
    current_node: NodeId,
    state: S::Checkpoint,  // P0-1: 投影
    graph_hash: u64,
    created_at: SystemTime,
}

// 4b. SessionCheckpoint<S> — 完整会话检查点
//     用于 ExecutionSession，支持持久化恢复
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
    pub reasoning_tokens: usize,
    pub compact_count: usize,
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
            reasoning_tokens: self.reasoning_tokens,
            compact_count: self.compact_count,
            stop_reason: self.stop_reason.clone(),
            total_tool_calls: self.total_tool_calls,
        }
    }

    fn restore(checkpoint: AgentCheckpoint) -> Self {
        AgentState {
            messages: checkpoint.messages,
            iterations: checkpoint.iterations,
            output_tokens: checkpoint.output_tokens,
            reasoning_tokens: checkpoint.reasoning_tokens,
            compact_count: checkpoint.compact_count,
            stop_reason: checkpoint.stop_reason,
            last_response: None,  // 重建时为空，下次 LLM 调用会填充
            total_tool_calls: checkpoint.total_tool_calls,
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

**核心问题**：早期 `Graph.nodes` 使用 `HashMap`，迭代顺序不确定，导致 `graph_hash` 不稳定。
**当前已修复**：`Graph.nodes` 已切换为 `IndexMap`（插入顺序稳定），但 DSL 层仍然应当计算 canonical_hash 并显式设置，而非依赖构建顺序。

**问题场景**：

```rust
// 早期问题：Graph.nodes 使用 HashMap，迭代顺序不确定
// graph1.nodes: {"llm" → ..., "tool" → ..., "budget_check" → ...}
// graph2.nodes: {"budget_check" → ..., "llm" → ..., "tool" → ...}  // 顺序可能不同
// hash1 ≠ hash2 → checkpoint 失效！
//
// 当前 Graph.nodes 已切换为 IndexMap（插入顺序稳定），
// 但 DSL 层仍然计算 canonical_hash 并显式设置，不依赖构建顺序。
```

**目标设计**：Graph Hash 来自 DSL 层的 canonical AST，不依赖 compiled graph 的节点顺序。

**两层 Hash 机制**：
- **DSL 层**（AgentBuilder）：计算基于 DSL 输入的 canonical hash，通过 `builder.canonical_hash(hash)` 设置
- **Primitive 层**（GraphBuilder）：如果不设置，`build()` 自动计算结构 hash（排序节点名 + 边）

```rust
// lellm-agent/src/runtime/builder.rs
impl AgentBuilder {
    /// 计算 canonical AST hash — 不依赖 NodeId 顺序。
    ///
    /// Hash 输入：
    /// - model provider + model name
    /// - tool names（保持 DSL 插入顺序，见 D11）
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

        // 2. Tools（保持 DSL 插入顺序，见 D11）
        //    .tools([A, B]) != .tools([B, A])
        for t in &self.static_tools {
            t.name.hash(&mut hasher);
        }

        // 3. System prompt (hash 内容)
        if let Some(ref system) = self.config.system {
            // system.hash() 需要稳定实现
            format!("{:?}", system).hash(&mut hasher);
        }

        // 4. 结构性配置
        self.config.max_iterations.hash(&mut hasher);
        self.config.max_output_tokens.hash(&mut hasher);
        self.config.max_total_output_tokens.hash(&mut hasher);
        self.config.max_total_reasoning_tokens.hash(&mut hasher);

        hasher.finish()
    }
}

// Graph 携带 canonical hash
pub struct Graph<S: WorkflowState, M: MergeStrategy<S>> {
    pub(crate) name: String,
    pub(crate) nodes: IndexMap<String, NodeKind<S, M>>,  // 插入顺序稳定
    pub(crate) edges: Vec<Edge<S>>,
    pub(crate) start: String,
    pub(crate) end: String,
    pub(crate) canonical_hash: u64,  // 来自 DSL，不来自 nodes 顺序
}

// Checkpoint 使用 canonical hash
impl<S: WorkflowState> Checkpoint<S> {
    pub fn new(current_node: impl Into<String>, state: &S, graph_hash: u64) -> Self {
        Self {
            checkpoint_id: CheckpointId(uuid::Uuid::new_v4()),
            current_node: NodeId(current_node.into()),
            state: state.snapshot(),  // P0-1: 使用 snapshot() 投影
            graph_hash,
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

### D10：ExecutionSession 持有 Arc\<Graph\>

**结论**：✅ 已实现

**设计原则**：Runtime 尽量共享不可变对象。

Graph 是 Immutable 的，多个 Session 共享同一个 Graph 实例：

```text
Runtime
└── Arc<Graph>

Session1 ──┐
Session2 ──┼── Arc<Graph>
Session3 ──┘
```

**实现**：
- `AgentBuilder::build()` 返回 `Arc<Graph<AgentState>>`
- `ExecutionSession` 持有 `Arc<Graph<S, M>>`
- `ToolUseLoop` 持有 `Arc<Graph<AgentState, AgentStateMerge>>`
- 恢复时：`ExecutionSession::restore(checkpoint, graph.clone())`

**不需要 GraphRegistry**：v0.5 场景下，调用方直接持有 `Arc<Graph>` 传入即可。

### D11：canonical_hash 保持 DSL 原貌

**结论**：❌ 不排序，保持 DSL 输入顺序

**设计原则**：Checkpoint 尽量精确反映用户构建的 Runtime。

canonical = DSL 原貌，不做语义归一化：

```rust
// 以下两种写法产生不同 hash
AgentBuilder::new(model).tools([a, b]).canonical_hash()  // hash1
AgentBuilder::new(model).tools([b, a]).canonical_hash()  // hash2 ≠ hash1
```

**原因**：
1. Graph Hash 的作用是判断 Checkpoint 是否能**完全一致**地继续执行
2. 工具顺序可能影响 prompt（如 `tool_choice`, `parallel_tool_calls`）
3. 不应该替用户假设"工具顺序没有影响"
4. 如果用户想忽略顺序，应该在 Catalog 层处理，不是在 Hash 层

**类比**：类似 `Cargo.lock`, `Dockerfile` — 都是输入 hash，不做 normalize。

## 与 LangGraph 的对比

| 维度 | LangGraph | LeLLM (v0.5) |
|------|-----------|-------------|
| 唯一 Runtime | StateGraph + compile | ExecutionEngine |
| Agent 定义 | create_react_agent() (黑盒) | AgentBuilder → Graph |
| 便捷执行 | graph.invoke() | ToolUseLoop.invoke() (薄 Facade) |
| 自定义 Agent | StateGraph 手写 | GraphBuilder 手写 / SubgraphSpec |
| 组合 | 子图节点 | SubgraphSpec → CompiledSubgraph (递归执行) |
| Checkpoint | 单层 | 单层 ✅ |
| Graph Factory 抽象 | 无 | 无（保持命名约定） |

## 收益

1. **AgentBuilder::build() → Arc\<Graph\>** — 消除双重 Runtime，架构统一（最大收益）✅
2. **统一 ExecutionEngine** — Checkpoint/Trace/Cancellation/Streaming 全部单层 ✅
3. **ToolUseLoop 重构为薄 Facade** — 持有预构建 Graph，不再每次重新构建 ✅
4. **删除 AgentFlowNode** — 减少代码量，消除反模式 ✅
5. **保留 AgentBuilder** — 保持 DSL 价值，build_react_graph() 保持私有 ✅
6. **不做 GraphFactory Trait** — 符合 Rust 风格，统一命名约定 ✅
7. **不做 GraphBuilder::merge()** — Subgraph 作为原语，merge 作为 Compiler Pass ✅
8. **Compiler Pass 框架** — `compile()` 方法已接入，InlinePass 为骨架实现 ⏸️
9. **Checkpoint Projection（P0-1）** — `type Checkpoint` 关联类型，强制 projection，序列化安全 ✅
10. **Graph Hash 稳定性（P0-2）** — 从 DSL canonical form 计算，保持输入顺序 ✅
11. **FrameStack 归属修正** — Engine 不持有 FrameStack，职责分离更清晰 ✅
12. **Arc\<Graph\> 共享（D10）** — Graph 是 Immutable 的，多 Session 共享同一实例 ✅

## 实现状态

- [x] Phase 1：AgentBuilder::build() → Arc\<Graph\<AgentState\>\>
- [x] Phase 2：ToolUseLoop 重构为薄 Facade
- [x] Phase 3：删除 AgentFlowNode
- [x] Phase 4：Subgraph 统一 — StateProjector + CompiledSubgraph + Engine dispatch
- [ ] Phase 5：Compiler Inline Pass（骨架实现，仅识别 + 统计，未执行实际内联）
- [x] Phase 6：Checkpoint = Execution Frame Snapshot
- [x] Phase 7：P0-1 Checkpoint Projection — `type Checkpoint` 关联类型
- [x] Phase 8：P0-2 Graph Hash — canonical AST hash
- [x] Phase 9：ExecutionSession — FrameStack 归属修正
- [x] Phase 10：D10 Arc\<Graph\> — 共享 Immutable 对象（AgentBuilder/ToolUseLoop/ExecutionSession/SubgraphSpec 统一 Arc）
- [x] Phase 11：D11 DSL 原貌 Hash — 保持输入顺序

> Status: Implemented (v0.5)

## 时间线

已完成：Phase 1 ~ Phase 4, Phase 6 ~ Phase 11 全部完成
进行中：Phase 5（Compiler Inline Pass 骨架实现，`compile()` 已接入流水线但内联逻辑未实现）

v0.5 架构重构基本完成，P0 设计补丁已落地！

---

## 附录：grill-me 讨论记录

### 关键决策点

1. **GraphFactory Trait** → 去掉，保持命名约定
2. **ToolUseLoop** → 重构为持有预构建 Graph 的 Facade
3. **GraphBuilder::merge()** → 不实现，Subgraph 作为原语，merge 作为 Compiler Pass
4. **Subgraph 统一** → 删除 SubgraphNode trait，使用 StateProjector + CompiledSubgraph 类型擦除；Engine 提供唯一 execute_subgraph() 实现
5. **StateLens vs StateAdapter** → 选择 StateLens，零拷贝投影
6. **Checkpoint** → 通过 `state.snapshot()` 获取投影快照，不依赖 Engine 持有所有权
7. **ExecutionContext 所有权** → Engine 借用 State（`&'a mut S`），调用方持有所有权。Parallel 分支使用 `OwnedExecutionEngine<S>`
8. **P0-1 Checkpoint Projection** → 引入 `type Checkpoint` 关联类型，强制 projection，序列化安全
9. **P0-2 Graph Hash** → 从 DSL canonical form 计算，保持输入顺序
10. **FrameStack 归属** → Engine 不持有 FrameStack，职责分离到 ExecutionSession
11. **Arc\<Graph\>** → Immutable 对象共享，多 Session 共享同一实例
12. **DSL 原貌 Hash** → 不做语义归一化，精确反映用户构建

### 最终结论

- **两层世界划分**：DSL（稳定）和 Primitive（完全自由）
- **不做中间层**：避免 "半开放 ReAct Graph"
- **统一产物**：所有 Builder 都返回 Arc\<Graph\<S\>\>
- **统一 Runtime**：只有一个 ExecutionEngine
- **零拷贝组合**：通过 StateLens 投影状态，不需要 clone/merge
- **Subgraph 是 Composite Node**：AST 中是 `NodeKind::Subgraph(CompiledSubgraph<S>)`；Engine 提供唯一 execute_subgraph() 实现
- **Engine 借用 State**：`ExecutionEngine<'a, S>` 持有 `&'a mut S`，调用方持有所有权
- **Checkpoint = snapshot()**：通过 `type Checkpoint` 关联类型强制 projection
- **Graph = Arc**：Immutable 对象共享，不拷贝
- **Hash = DSL 原貌**：不做语义归一化，精确反映用户构建
- **Subgraph 类型擦除**：StateProjector trait 擦除 Inner/Lens/M，保留 Outer
