# LeLLM v0.5 产品蓝图

> 版本：v0.5 | 日期：2026-07-01 | 状态：Graph is Runtime, Agent is DSL
> 设计决策详见 [v05-graph-as-runtime.md](./v05-graph-as-runtime.md)

## 一、项目愿景

做 Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，标准化消息内容格式；提供基础的 LLM provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use, MCP client
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 二、v0.5 架构

### 核心论点

**Graph 是唯一的 Runtime，Agent 是 Graph 的 DSL / 模板构建器。**

### 6 Crate 架构

```
         lellm
           │
     ┌─────┼─────┬─────┐
     ▼     ▼     ▼     ▼
   graph  agent  mcp  derive
     │     │     │
     ▼     ▼     ▼
   core  provider core
```

### Crate 职责

| Crate | 领域 | 职责 | 依赖 |
|-------|------|------|------|
| `lellm-core` | Protocol | 纯协议层：Message, ToolCall, Request/Response, LlmError | serde, thiserror |
| `lellm-graph` | Execution | 图编排引擎：Graph, Node, Edge, State, Checkpoint, ExecutionEngine, ExecutionSession | core |
| `lellm-provider` | Inference | LLM 调用：LlmProvider, CodecProvider, 三权分立 | core |
| `lellm-agent` | Agent | 智能体：AgentBuilder, ToolUseLoop, AgentState | core, graph, provider |
| `lellm-mcp` | Protocol | MCP 协议：McpClient, McpTransport | core |
| `lellm-derive` | Technical | 派生宏：#[tool], #[derive(Tool)] | 无 |

### 红线

1. `graph ↛ agent` — Graph 是通用引擎，Agent 是上层消费者
2. `provider ↛ graph` — Provider 只负责 LLM 调用
3. `mcp ↛ agent` — MCP 是独立协议域

### Feature Gate

```toml
[features]
default = ["provider"]
core = ["dep:lellm-core"]
provider = ["dep:lellm-core", "dep:lellm-provider"]
graph = ["dep:lellm-core", "dep:lellm-graph"]
agent = ["dep:lellm-core", "dep:lellm-graph", "dep:lellm-provider", "dep:lellm-agent"]
mcp = ["dep:lellm-core", "dep:lellm-graph", "dep:lellm-mcp"]
derive = ["dep:lellm-derive"]
full = ["graph", "provider", "agent", "mcp", "derive"]
```

## 三、Workspace 结构

```
lellm/
├── Cargo.toml                  # workspace root
├── lellm/                      # 门面 crate — feature-gated re-export
├── lellm-core/                 # 协议层，零运行时依赖
├── lellm-graph/                # 图编排引擎 + State + Checkpoint + ExecutionSession
├── lellm-provider/             # LLM Provider trait + 适配器
├── lellm-agent/                # Agent 运行时 (AgentBuilder, ToolUseLoop)
├── lellm-mcp/                  # MCP 协议实现
├── lellm-derive/               # 派生宏
└── docs/                       # 文档
```

## 四、架构总览

```
用户层
──────────────────────────────────────
AgentBuilder  PlannerBuilder  SupervisorBuilder
                    (DSL, build() → Arc<Graph<S>>)

                    ↓ build()
               Arc<Graph<AgentState>>

                    ┌──────────────────────┐
                    │  ToolUseLoop (Facade) │  ← 薄层，持有 Arc<Graph>
                    │  invoke()             │     invoke_stream()
                    │  invoke_stream()      │
                    └──────────────────────┘

                    ↓ 组合

               SubgraphSpec (Engine 行为，不是 Node)

──────────────────────────────────────
          Runtime (lellm-graph)
ExecutionEngine  Node  Edge  State  Graph  ExecutionSession

──────────────────────────────────────
          Primitive (lellm-graph)
GraphBuilder  (AST 构建器，build() → Graph)

──────────────────────────────────────
          Compiler (lellm-graph, 可选优化)
Inline Pass  (自动 merge Subgraph，用户不需要手动调用)
```

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

## 五、核心 API

### 5.1 LlmProvider

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError>;
    fn provider_id(&self) -> &str;
}
```

### 5.2 WorkflowState

```rust
pub trait WorkflowState: Clone + Send + Sync {
    /// 可序列化的 Checkpoint 快照（projection，不是 raw state）
    type Checkpoint: Serialize + DeserializeOwned + Clone + Send;
    /// 状态变更命令
    type Mutation: StateMutation<Self>;

    /// 创建 checkpoint 快照
    fn snapshot(&self) -> Self::Checkpoint;
    /// 从 checkpoint 恢复
    fn restore(checkpoint: Self::Checkpoint) -> Self;
    /// 批量应用 Mutation
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = Self::Mutation>);
}
```

### 5.3 AgentBuilder (DSL)

```rust
// 构建 Graph — 推荐方式
let graph: Arc<Graph<AgentState>> = AgentBuilder::new(model)
    .system("你是一个助手")
    .tools([search_tool, weather_tool])
    .max_iterations(20)
    .build();

// 便捷执行 — Facade
let result = AgentBuilder::new(model)
    .tools([search_tool, weather_tool])
    .build_loop()
    .invoke(messages)
    .await?;
```

### 5.4 GraphBuilder (Primitive)

```rust
let graph = GraphBuilder::<MyState, _>::new("workflow")
    .start("fetch")
    .node("fetch", fetch_node)
    .node("process", process_node)
    .edge("fetch", "process")
    .end("process")
    .build()?;

// 执行
let mut engine = ExecutionEngine::new(&mut state, None, CancellationToken::new());
graph.run_inline(&mut engine, 100).await?;
```

### 5.5 ExecutionSession

```rust
// 创建 Session
let mut session = ExecutionSession::new(state, graph.clone());

// 创建 Checkpoint
let checkpoint = session.checkpoint();

// 从 Checkpoint 恢复
let session = ExecutionSession::restore(checkpoint, graph);

// 执行
let mut engine = ExecutionEngine::new(&mut session.state, Some(stream), cancel);
session.run_with(&mut engine).await?;
```

## 六、关键设计决策

| 主题 | 说明 |
|------|------|
| Graph is Runtime | Graph 是唯一的 Runtime，Agent 是 DSL |
| Checkpoint Projection | `type Checkpoint` 关联类型，Runtime State 与 Checkpoint 分离 |
| Graph Hash | 从 DSL canonical form 计算，保持输入顺序，不做语义归一化 |
| ExecutionSession | 持有 State 所有权 + FrameStack + Arc\<Graph\> |
| StateLens | 零拷贝状态投影，用于 Subgraph 组合 |
| Subgraph | Graph AST 的一种节点，Runtime 中递归执行 |
| Engine 借用 State | `ExecutionEngine<'a, S>` 持有 `&'a mut S`，调用方持有所有权 |

## 七、版本路线图

| 版本 | 范围 |
|------|------|
| **v0.1** | core + provider + agent + macros + MCP (Tools only) |
| **v0.2** | Graph/Node/Edge + 有环图 + BarrierNode + 流式执行 + 错误二分法 |
| **v0.3** | 6 crate 架构重构 + StateDelta + Checkpoint + ParallelNode + MCP |
| **v0.4** | ReAct = 有环图 + Typed State + Mutation 事件溯源 |
| **v0.5** | ✅ Graph is Runtime + Agent is DSL + Checkpoint Projection + ExecutionSession |
| **v0.6** | Multi-Agent Orchestration + Durable Execution + Human-in-the-loop |
| **v0.7** | Sampling |
