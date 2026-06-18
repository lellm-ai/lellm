# LeLLM v0.3 产品蓝图

> 版本：v0.3 | 日期：2026-06-18 | 状态：代码已对齐
> 设计决策详见 [DESIGN.md](./DESIGN.md) / [v03-architecture-evolution.md](./v03-architecture-evolution.md)

## 一、项目愿景

做 Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，标准化消息内容格式；提供基础的 LLM provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use, MCP client
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration（v0.4+）
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 二、v0.3 架构

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
| `lellm-graph` | Execution | 图编排引擎：Graph, Node, Edge, State, StateDelta, Checkpoint, Events | core |
| `lellm-provider` | Inference | LLM 调用：LlmProvider, CodecProvider, 三权分立 | core |
| `lellm-agent` | Agent | 智能体：ToolUseLoop, AgentEvent, AgentFlowNode | core, graph, provider |
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
├── lellm-graph/                # 图编排引擎 + State + Checkpoint
├── lellm-provider/             # LLM Provider trait + 适配器
├── lellm-agent/                # Agent 运行时
├── lellm-mcp/                  # MCP 协议实现
├── lellm-derive/               # 派生宏
└── docs/                       # 文档
```

## 四、架构总览

```
用户
 ↓
Graph (编排引擎)
 ↓
FlowNode (trait)
 ↓
├─ AgentFlowNode (Agent 适配器)
│    ↓
│  Agent (ToolUseLoop)
│    ↓
│  LlmProvider → 外部 LLM
│
├─ TaskNode (简单任务)
├─ ConditionNode (条件分支)
├─ BarrierNode (人类介入)
└─ ParallelNode (并行执行)
```

## 五、核心 API

### 5.1 LlmProvider

```rust
pub trait LlmProvider: Send + Sync {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError>;
    fn provider_id(&self) -> &str;
}
```

### 5.2 FlowNode

```rust
#[async_trait]
pub trait FlowNode: Send + Sync {
    async fn execute(
        &self,
        ctx: &mut FlowContext,
    ) -> Result<NodeOutput, GraphError>;
}
```

### 5.3 Graph

```rust
let graph = GraphBuilder::new("my_workflow")
    .start("fetch")
    .node("fetch", fetch_node)
    .node("process", process_node)
    .edge("fetch", "process", Always)
    .end("process")
    .build();

let result = graph.execute(initial_state).await?;
```

### 5.4 Agent

```rust
let agent = AgentBuilder::new(model)
    .system_prompt("...".into())
    .tool(search_tool)
    .build();

let result = agent.execute(messages).await?;
```

## 六、关键设计决策

| 主题 | 说明 |
|------|------|
| TraceId/SpanId | 从 core 迁移到 graph，作为执行引擎的一部分 |
| StateDelta | 节点输出 Delta，不直接修改 State。Executor 统一 apply |
| Checkpoint | 默认每步触发，支持增量快照 |
| 并行执行 | 分支隔离 + Reducer 合并 |
| Barrier | 必须配置超时，编译期强制 |
| 流式输出 | 单一 Stream，事件透传 |
| Error 策略 | 可配置 FailFast / BestEffort |
| 循环保护 | 只靠 max_steps |

## 七、版本路线图

| 版本 | 范围 |
|------|------|
| **v0.1** | core + provider + agent + macros + MCP (Tools only) |
| **v0.2** | Graph/Node/Edge + 有环图 + BarrierNode + 流式执行 + 错误二分法 |
| **v0.3** | 6 crate 架构重构 + StateDelta + Checkpoint + ParallelNode + MCP |
| **v0.4** | Multi-Agent Orchestration + Durable Execution + Agent 内部基于 Graph |
| **v0.5** | Sampling + Agent↔Agent via MCP |
