# LeLLM v0.3 架构演进

> 版本：v0.3 | 日期：2026-06-18 | 状态：已完成 ✅
> 后续演进：[v04-architecture-evolution.md](./v04-architecture-evolution.md)
>
> 本文档记录 v0.3 所有设计决策和实现细节。

## 目录

- [一、架构总览](#一架构总览)
- [二、Crate 设计](#二crate-设计)
- [三、核心类型](#三核心类型)
- [四、Graph 编排引擎](#四graph-编排引擎)
- [五、Agent 运行时](#五agent-运行时)
- [六、Provider 适配层](#六provider-适配层)
- [七、MCP 协议](#七mcp-协议)
- [八、关键设计决策](#八关键设计决策)
- [九、实施计划](#九实施计划)

---

## 一、架构总览

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

---

## 二、Crate 设计

### lellm-core（Protocol Layer）

纯协议层，零运行时依赖。

| 类型 | 说明 |
|------|------|
| `Message` | 对话消息 |
| `ContentBlock` | 消息内容块（Text/Thinking/Image/ToolCall）|
| `ChatRequest` / `ChatResponse` | LLM 请求/响应 |
| `ToolCall` / `ToolDefinition` | 工具调用/定义 |
| `LlmError` | LLM 错误类型 |
| `ToolError` / `ToolResult` | 工具执行错误/结果 |

依赖：serde, serde_json, thiserror, schemars

### lellm-graph（Execution Layer）

图编排引擎，吸收了原 lellm-runtime。

| 模块 | 职责 |
|------|------|
| `graph` | Graph, GraphBuilder, Edge |
| `node` | FlowNode trait, TaskNode, ConditionNode, BarrierNode, ParallelNode |
| `executor` | GraphExecutor（阻塞/流式执行）|
| `state` | State, StateExt, StateError |
| `delta` | StateDelta, DeltaOp, Reducer, ReducerRegistry |
| `statekey` | StateKey, StateKeyExt, 内置常量 |
| `checkpoint` | Checkpoint, CheckpointStore, CheckpointPolicy |
| `store` | InMemoryCheckpointStore |
| `ids` | TraceId, SpanId |
| `event` | GraphEvent, FlowEvent, BarrierId, BarrierDecision |
| `error` | GraphError, BuildError, ObservedError |
| `hook` | AgentHook trait |

依赖：lellm-core

### lellm-provider（Inference Layer）

LLM 调用适配层，三权分立设计。

| 类型 | 职责 |
|------|------|
| `LlmProvider` | 统一的 LLM Provider trait |
| `CodecProvider` | 持有 Codec + 连接配置 |
| `ChatCodec` | 协议编解码 |
| `ModelCapabilities` | 模型能力矩阵 |
| `ProviderMeta` | 连接元数据 |
| `ModelRouter` | 任务分级路由 |
| `ProviderRegistry` | Provider 注册表 |

支持的 Provider：OpenAI, Anthropic, Google, OpenAI-Compatible

依赖：lellm-core

### lellm-agent（Agent Layer）

智能体运行时。

| 类型 | 职责 |
|------|------|
| `ToolUseLoop` | Agent 循环（LLM 调用 + 工具执行）|
| `AgentBuilder` | Agent 构建器 |
| `AgentEvent` | Agent 事件（ToolStart/ToolEnd/LoopEnd 等）|
| `AgentFlowNode` | Agent 适配为 Graph 节点 |
| `ToolExecutor` | 工具执行器 |
| `ToolCatalog` | 工具目录 |
| `RetryPolicy` | 重试策略 |
| `FallbackStrategy` | 降级策略 |
| `ContextCompactor` | 上下文压缩 |

依赖：lellm-core, lellm-graph, lellm-provider

### lellm-mcp（Protocol Layer）

MCP 协议实现。

| 类型 | 职责 |
|------|------|
| `McpClient` | MCP 客户端 |
| `McpTransport` | 传输层（stdio）|
| `McpCatalog` | 工具发现 |

依赖：lellm-core

### lellm-derive（Technical Layer）

派生宏。

| 宏 | 职责 |
|------|------|
| `#[tool]` | 函数宏，自动生成 ToolRegistration |
| `#[derive(Tool)]` | 结构体宏，自动生成 ToolDefinition |

依赖：无

---

## 三、核心类型

### StateDelta — 状态增量模型

```rust
pub struct StateDelta {
    pub key: Cow<'static, str>,
    pub op: DeltaOp,        // Put | Delete
    pub value: Value,
    pub source: DeltaSource,
}
```

节点输出 Delta，不直接修改 State。Executor 收集所有 Delta 后统一 apply。

### Reducer — 合并策略

```rust
pub enum Reducer {
    Error,        // 冲突即报错
    Replace,      // 最后写入者胜
    Append,       // 数组追加
    MergeObject,  // 对象浅合并
    Sum,          // 数值求和
    Max,          // 取最大值
    Min,          // 取最小值
    Custom(fn),   // 自定义合并函数
}
```

### Checkpoint — 持久化执行

```rust
pub struct Checkpoint {
    pub checkpoint_id: CheckpointId,
    pub parent_trace_id: TraceId,
    pub graph_hash: String,
    pub current_node: NodeId,
    pub state: State,
    pub created_at: String,
    pub snapshot: Option<StateSnapshot>,
}
```

默认每步触发，支持增量快照。

---

## 四、Graph 编排引擎

### FlowNode trait

```rust
#[async_trait]
pub trait FlowNode: Send + Sync {
    async fn execute(
        &self,
        ctx: &mut FlowContext,
    ) -> Result<NodeOutput, GraphError>;
}
```

### NodeOutput — 节点输出

```rust
pub struct NodeOutput {
    pub deltas: Vec<StateDelta>,
    pub next: NextStep,
    pub metadata: Option<NodeMetadata>,
}
```

### Edge — 三类边模型

1. **条件边** (`edge_if`) — if/else-if 规则链
2. **普通边** (`edge`) — 无条件非 fallback
3. **Fallback 边** (`edge_fallback`) — 最后兜底

### 执行模式

- **阻塞执行**：`graph.execute(state).await`
- **流式执行**：`graph.execute_stream(state)` → `GraphExecution { stream, handle }`

---

## 五、Agent 运行时

### AgentEvent

```rust
pub enum AgentEvent {
    Provider(ProviderEvent),
    ToolStart { tool_call_id, name },
    ToolEnd { tool_call_id, result },
    Retry { tool_call_id, attempt, reason },
    ContextCompacted { before_tokens, after_tokens },
    LoopEnd { result: LoopEndResult },
    LoopError { error, iterations },
}
```

### AgentFlowNode — Agent 适配 Graph

```rust
impl FlowNode for AgentFlowNode {
    async fn execute(&self, ctx: &mut FlowContext) -> Result<NodeOutput, GraphError> {
        // 1. 从 State 提取 messages
        // 2. 调用 ToolUseLoop.execute()
        // 3. 将结果写入 StateDelta
        // 4. 返回 NodeOutput
    }
}
```

---

## 六、Provider 适配层

### 三权分立

```
用户 → LlmProvider (public API)
       → CodecProvider<C> (框架内部)
          → ProviderExtension 三权分立 (生态扩展 SPI)
              ├── ChatCodec (协议编解码)
              ├── ModelCapabilities (能力矩阵)
              └── ProviderMeta (连接元数据)
```

### 新增 Provider

只需实现 `ProviderExtension` trait（组合三个子 trait），无需修改框架代码。

---

## 七、MCP 协议

### McpClient

```rust
let client = McpClient::new(transport);
client.initialize().await?;
let tools = client.list_tools().await?;
let result = client.call_tool("search", args).await?;
```

### ToolBridge

MCP 工具 → Agent 工具适配器：

```rust
let catalog = McpCatalog::new(client);
let tools: Vec<ToolRegistration> = catalog.discover().await?;
agent.tools(tools).build();
```

---

## 八、关键设计决策

| 决策 | 结论 | 理由 |
|------|------|------|
| TraceId/SpanId 归属 | graph | core 是纯协议层，不应包含 trace 概念 |
| State 实现 | 扁平 KV | 简单，易于序列化 |
| Checkpoint 触发 | 默认每步 | 开箱即用 |
| 并行执行 | 分支隔离 + Reducer | 避免竞态 |
| Barrier 超时 | 必须配置 | 防止运行时阻塞 |
| 流式输出 | 单一 Stream | 简化消费者 |
| Error 策略 | 可配置 | 不同节点不同容错 |
| 循环保护 | 只靠 max_steps | 简单可靠 |
| Feature Gate | default=provider | 按需启用 |
| Provider 内部 | 三权分立 | 扩展性 |

---

## 九、实施计划

### 已完成阶段

| 阶段 | 目标 | 状态 |
|------|------|------|
| 1 | `lellm-core` 清理 — 移除 TraceId/SpanId | ✅ |
| 2 | `lellm-graph` 吸收 `lellm-runtime` + `lellm-events` | ✅ |
| 3 | `lellm-provider` 清理 | ✅ |
| 4 | `lellm-mcp` 保持现状 | ✅ |
| 5 | `lellm-agent` 清理 | ✅ |
| 6 | `lellm-macros` → `lellm-derive` 更名 | ✅ |
| 7 | `lellm` facade 重构 | ✅ |

### 测试结果
- 223 个测试全部通过
- 全量编译通过
- cargo fmt 通过

### 未来规划

详见 [v04-architecture-evolution.md](./v04-architecture-evolution.md)。

| 版本 | 范围 |
|------|------|
| v0.3 收尾 | 消灭 LoopState → 统一 StateKey（方案 B+）→ 单一事实来源 |
| v0.4 | ReAct = 有环图 + Typed State + Effect 事件溯源 + Workflow\<S\> |
| v0.5 | Multi-Agent Orchestration + Durable Execution + Agent↔Agent via MCP |
| v0.6 | Sampling |
