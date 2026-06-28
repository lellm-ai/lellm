# LeLLM v0.3 架构演进

> 版本：v0.3 | 日期：2026-06-19 | 状态：v0.3.1 实施中 🔄
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
- [v0.3.1、消灭双来源状态](#v031消灭双来源状态)
- [十、实施计划](#十实施计划)

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
| LoopState 去留 | 消灭 | 双来源状态 = Bug 温床 |
| StateKey 设计 | 5 个核心键 + Runtime Cache | 最小集，语义明确 |
| StopReason 归属 | Control Plane | 不属于可持久化业务状态 |
| compact 触发 | Runtime Cache |派生数据不进 State，用 cached_token_count 判断 |
| SK_TOOL_CALLS 语义 | 重命名为 SK_PENDING_TOOL_CALLS | 当前轮 pending，非历史累计 |

---

## v0.3.1、消灭双来源状态

### 问题

当前 `ToolUseLoop` 持有私有 `LoopState`（`Vec<Message>`, `estimated_tokens`, `iterations` 等），
同时 Graph 层有自己的 `State = HashMap<String, Value>`。
**双来源状态 = Bug 温床。**

### 三层数据模型

| 层 | 职责 | 示例 |
|---|---|---|
| **State** | 可持久化业务状态 | SK_MESSAGES, SK_ITERATIONS |
| **Runtime Cache** | 执行期性能优化 | cached_token_count |
| **Control Signal** | 路由决策 | StopReason, NeedCompact |

### 决策：方案 B+（统一状态来源）

`ToolUseLoop` 不再持有任何私有状态。所有 Agent 状态全部摊在 Graph State 中：

```rust
// 核心状态键收拢，成为底层图的公共契约
pub static SK_MESSAGES: StateKey<Vec<Message>> = StateKey::append("messages");
pub static SK_ITERATIONS: StateKey<u32> = StateKey::replace("iterations");
pub static SK_PENDING_TOOL_CALLS: StateKey<Vec<ToolCall>> = StateKey::replace("pending_tool_calls");
pub static SK_OUTPUT_TOKENS: StateKey<usize> = StateKey::sum("output_tokens");
pub static SK_REASONING_TOKENS: StateKey<usize> = StateKey::sum("reasoning_tokens");
```

### 从 LoopState 的映射

| LoopState 字段 | 目标 | 处理 |
|---|---|---|
| `messages` | `SK_MESSAGES` | 直接迁移 |
| `iterations` | `SK_ITERATIONS` | 直接迁移 |
| `tool_calls_executed` | `SK_PENDING_TOOL_CALLS` | 语义重定义为当前轮 pending |
| `total_output_tokens` | `SK_OUTPUT_TOKENS` | 直接迁移 |
| `total_reasoning_tokens` | `SK_REASONING_TOKENS` | 直接迁移 |
| `estimated_tokens` | Runtime Cache | 派生数据，不进 State |

### Runtime Cache：AgentExecutionContext

```rust
pub struct AgentExecutionContext {
    cached_token_count: usize,
}
```

- 由 `AgentFlowNode` 持有，传给 `ToolUseLoop`
- compact 时用缓存判断阈值，不实时计算
- Checkpoint 不保存，Resume 时重建

### 不做的事（留给 v04）

- `AgentFlowNode` 不改成 SubGraph（v04 ReAct = 有环图）
- `SK_STOP_REASON` 不引入（属于 Control Plane）
- `SK_TOOL_CALL_HISTORY` 不引入（v04 审计需求）
- `compact()` 不改成 Graph Node（v04 Agent = Internal Graph）
- **不统一流式/非流式路径** — execute() 和 execute_stream() 保留两个入口，execute_iteration() + EventSink 留给 v04
- **不引入 StreamAggregationContext** — 流式聚合逻辑保持现状，v04 统一执行模型时再拆分

### 实现路径

#### Step 1：定义 Agent 层 StateKey 常量

文件：`lellm-graph/src/statekey.rs`

```rust
// Agent 核心状态键（v03 收尾）
pub static SK_MESSAGES: StateKey<Vec<serde_json::Value>> =
    StateKey::new("messages", Reducer::Append);
pub static SK_ITERATIONS: StateKey<u32> = StateKey::replace("iterations");
pub static SK_PENDING_TOOL_CALLS: StateKey<Vec<serde_json::Value>> =
    StateKey::replace("pending_tool_calls");
pub static SK_OUTPUT_TOKENS: StateKey<usize> = StateKey::sum("output_tokens");
pub static SK_REASONING_TOKENS: StateKey<usize> = StateKey::sum("reasoning_tokens");
```

> 注意：`SK_MESSAGES` 类型保持 `Vec<serde_json::Value>` 以兼容现有 Graph State 的 JSON 序列化。
> `Vec<Message>` ↔ `Vec<serde_json::Value>` 的转换在 Agent 层边界处理。

#### Step 2：引入 AgentExecutionContext

文件：`lellm-agent/src/runtime/context.rs`（新建）

```rust
/// Agent 运行时上下文 — 不可持久化的执行期缓存。
pub struct AgentExecutionContext {
    /// 消息历史的估算 Token 数（LRU cache 或简单累加）
    pub cached_token_count: usize,
}

impl AgentExecutionContext {
    pub fn new(messages: &[Message]) -> Self {
        Self {
            cached_token_count: estimate_tokens(messages),
        }
    }

    pub fn add_tokens(&mut self, tokens: usize) {
        self.cached_token_count += tokens;
    }

    pub fn reset_after_compact(&mut self, messages: &[Message]) {
        self.cached_token_count = estimate_tokens(messages);
    }
}
```

#### Step 3：删除 LoopState，ToolUseLoop 改为读写 State

文件：`lellm-agent/src/runtime/runtime.rs`

- 删除 `LoopState` 结构体及其 `impl` 块（第 57-226 行）
- `ToolUseLoop::execute()` 改为接收 `State` + `AgentExecutionContext`：

```rust
pub async fn execute(
    &self,
    state: &mut State,
    ctx: &mut AgentExecutionContext,
) -> Result<ToolUseResult, LlmError> {
    loop {
        // 从 State 读取
        let iterations: u32 = state.get_sk(&SK_ITERATIONS).unwrap_or(0);
        if iterations >= self.config.max_iterations as u32 {
            return Ok(/* finish_max_iterations */);
        }
        state.set_sk(&SK_ITERATIONS, iterations + 1);

        // compact（用 ctx.cached_token_count 判断）
        maybe_compact(state, ctx, &self.config.context_budget, &*compactor);

        // 构建请求 — 从 State 读 messages
        let messages = state.get_sk(&SK_MESSAGES).unwrap_or_default();
        let req = build_request(...);

        // 执行 LLM
        let response = self.model.provider.call(&req).await?;

        // 写入 State
        // ... push assistant message, tool calls, etc.
    }
}
```

#### Step 4：更新 AgentFlowNode

文件：`lellm-agent/src/runtime/flow_node.rs`

- `extract_messages()` 改为使用 `state.get_sk(&SK_MESSAGES)`
- `collect_deltas()` 改为写入标准 StateKey（不再用 `format!("{}_iterations", name)` 等自定义 key）
- 执行时创建 `AgentExecutionContext` 并传给 `ToolUseLoop`

```rust
async fn execute(&self, state: &State) -> Result<NodeOutput, GraphError> {
    let mut state = state.clone();
    let mut ctx = AgentExecutionContext::new(/* 从 state 读 messages */);

    let result = self.loop_.execute(&mut state, &mut ctx).await?;

    // 收集 Deltas — 使用标准 StateKey
    let deltas = vec![
        StateDelta::put_sk(&SK_MESSAGES, /* 最终 messages */),
        StateDelta::put_sk(&SK_ITERATIONS, result.iterations),
        StateDelta::put_sk(&SK_OUTPUT_TOKENS, ctx.cached_output_tokens),
        StateDelta::put_sk(&SK_REASONING_TOKENS, ctx.cached_reasoning_tokens),
    ];
    Ok(NodeOutput { deltas, next: NextStep::GoToNext, metadata: None })
}
```

#### Step 5：更新 iteration.rs

文件：`lellm-agent/src/runtime/iteration.rs`

- `process_stream_iteration` 参数从 `&mut LoopState` 改为 `(&mut State, &mut AgentExecutionContext)`
- `do_stream_iteration` 参数从 `LoopState` 改为 `(State, AgentExecutionContext)`
- 所有 `state.messages` → `state.get_sk(&SK_MESSAGES)`
- 所有 `state.iterations` → `state.get_sk(&SK_ITERATIONS)`
- 所有 `state.estimated_tokens` → `ctx.cached_token_count`

#### Step 6：更新 re-export

文件：`lellm-agent/src/runtime/mod.rs`

- 删除 `pub use runtime::LoopState`
- 新增 `pub use context::AgentExecutionContext`

#### Step 7：更新测试

文件：`lellm-agent/tests/*.rs`, `lellm-graph/tests/graph_test.rs`

- 现有 `SK_MESSAGES` 测试更新类型
- 新增 Agent 层 StateKey 的集成测试
- 验证 Checkpoint 能正确保存/恢复 Agent 中间状态

### 待做清单

- [ ] Step 1：定义 Agent 层 StateKey 常量
- [ ] Step 2：引入 AgentExecutionContext
- [ ] Step 3：删除 LoopState，ToolUseLoop 改为读写 State
- [ ] Step 4：更新 AgentFlowNode
- [ ] Step 5：更新 iteration.rs
- [ ] Step 6：更新 re-export
- [ ] Step 7：更新测试
- [ ] cargo fmt + cargo test 全量验证

---

## 十、实施计划

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
| 8 | v03 收尾 — 消灭 LoopState + 统一 StateKey | 🔄 |

### 测试结果
- 223 个测试全部通过
- 全量编译通过
- cargo fmt 通过

### 未来规划

#### v0.4：ReAct = 有环图

##### 问题

当前 `ToolUseLoop` 是一个手写的 `while` 循环（`runtime.rs:303-394`）：
LLM 调用 → 检查 tool_calls → 执行工具 → 追加消息 → 回到 LLM。

##### 决策：方案 B（中等粒度 Graph 建模）

```
[LLM Call] --有tool_calls--> [Execute Tools] --(自环)--> [LLM Call]
       --无tool_calls--> [End]
```

| 方案 | 描述 | 评价 |
|------|------|------|
| A（粗粒度） | 整个 ReAct 封装成单个节点，用自环替代 while | 过于敷衍，失去 Graph 能力 |
| **B（中等粒度）** | **LLM Node + Tool Node + 条件边** | **可观测性与灵活性的平衡** |
| C（细粒度） | 每步独立节点（LLM → Condition → Tool → Compactor） | 过度拆分，ReAct 内部紧密耦合 |

**方案 B — 直接替换：**
- `ToolUseLoop` 内部不再手写 `while` 循环
- 构建内部 Graph（LLM Node → Condition → Tool Node → 自环）
- 调用 `GraphExecutor` 驱动循环
- `ToolUseLoop` 变成一层薄壳，API 签名不变（用户无感知）

##### 内部 ReAct Graph 的 State 传递

基于 v0.3.1 的 5 个 StateKey + 新增 v0.4 专用键：

- `SK_MESSAGES` → 消息历史（v0.3.1 已定义）
- `SK_ITERATIONS` → 迭代计数（v0.3.1 已定义）
- `SK_PENDING_TOOL_CALLS` → 本轮工具调用（v0.3.1 已定义）
- `SK_OUTPUT_TOKENS` → 累计输出 Token（v0.3.1 已定义）
- `SK_REASONING_TOKENS` → 累计推理 Token（v0.3.1 已定义）
- `SK_TOOL_CALL_HISTORY` → 审计历史（v0.4 新增）

##### Agent 降维成 SubGraph

```
外部 Graph（用户编排）
  └── AgentFlowNode（Agent 适配为 Graph 节点）
        └── ToolUseLoop（薄壳）
              └── 内部 ReAct Graph（LLM ↔ Tool 循环）
```

##### 待做清单

- [ ] FlowNode 统一为 `execute(ctx) -> Result<(), GraphError>` — Context 驱动一切
- [ ] 消除 `NodeOutput`, `StreamNodeResult` — 所有数据写入 Context
- [ ] 引入 `ExecutionControl` + `ExecutionSignal` — 控制信号独立于路由
- [ ] Overlay State：`StateSnapshot` + `BranchState` 双层模型
- [ ] 设计 `LLMNode` — 执行单次 LLM 调用，`ctx.append(SK_MESSAGES, ...)` 
- [ ] 设计 `ToolNode` — 读取 tool_calls，执行工具，`ctx.append(SK_MESSAGES, ...)`
- [ ] `ToolUseLoop` 内部构建 ReAct Graph，替代 while 循环
- [ ] `AgentFlowNode` 简化为 SubGraph 包装器
- [ ] `compact()` 变成 Graph 中的 BudgetGuardNode + CompactNode
- [ ] 引入 `SK_TOOL_CALL_HISTORY`（审计历史）

#### v0.4+ 终局：Typed State + Mutation 事件溯源

v0.3.1 的 `HashMap<String, Value>` 是动态的、弱类型的。
`StateKey<T>` 和 `ReducerRegistry` 是补丁——在边界处做运行时类型检查。

##### 终局愿景：Workflow<S> + Mutation<S>

```rust
// 节点返回 Mutation 而非 Delta
pub enum AgentMutation {
    AppendMessage(Message),
    IncrementIteration,
    RecordUsage(TokenUsage),
}

// 状态机作为纯函数应用 Mutation
impl WorkflowState for AgentState {
    type Mutation = AgentMutation;
    fn apply(&mut self, mutation: Self::Mutation) {
        match mutation {
            AgentMutation::AppendMessage(msg) => self.messages.push(msg),
            AgentMutation::IncrementIteration => self.iterations += 1,
            AgentMutation::RecordUsage(usage) => self.usage += usage,
        }
    }
}

// 编译期 Merge 替代运行时 ReducerRegistry
pub trait Merge {
    fn merge(self, other: Self) -> Result<Self, WorkflowError>;
}
```

- **Checkpoint = Mutation Log**：追加轻量级 Mutation 到数据库，而非序列化几百 KB 的 JSON State
- **恢复**：重放 Mutation Log，天然支持确定性重放测试

#### 版本路线图

```
  v0.3 (收拢: 消灭 LoopState + 统一 StateKey)
  [SK_MESSAGES] [SK_ITERATIONS] [SK_PENDING_TOOL_CALLS]
  [SK_OUTPUT_TOKENS] [SK_REASONING_TOKENS]
  [AgentExecutionContext = Runtime Cache]
                                    │
                                    ▼
  v0.4 (破茧成蝶: 统一执行模型)
  [ReAct = 有环图] ──> [Agent 降维成 SubGraph]
  [Context 驱动一切] ──> [FlowNode.execute(ctx) -> Result<(), GraphError>]
  [Overlay State] ──> [StateSnapshot + BranchState]
  [ChangeLog 节点级别] ──> [Reducer merge changes]
  [Control/Data Plane 分离] ──> [RuntimeEvent + StreamChunk]
  [ExecutionControl + ExecutionSignal]
                                    │
                                    ▼
  v0.4+ (强类型领域)
  [砸碎 HashMap] ──> [Workflow<S>]
  [Mutation 事件溯源] ──> [编译期 Merge]
                                    │
                                    ▼
  v0.5 (多智能体时代)
  [Multi-Agent Orchestration] ──> [Durable Execution]
  [Agent ↔ Agent via MCP] ──> [Sampling]
```

#### 架构演进对比

| 维度 | v0.3.1 | v0.4 | v0.4+ (终极) |
|------|-------|------|-------------|
| **节点签名** | `execute(state) -> NodeOutput` | `execute(ctx) -> Result<(), GraphError>` | 同 v0.4 |
| **状态底层** | `HashMap<String, Value>` | 同 v0.3.1 | 强类型 `S` |
| **变更机制** | `StateDelta` | `ChangeRecord` (Overlay) | `Mutation<S>` |
| **并行合并** | 运行时 Reducer | Reducer merge changes | `Merge` trait 编译期 |
| **Checkpoint** | State 快照 | Snapshot + ChangeLog | Mutation Log 重放 |
| **中间类型** | `NodeOutput`, `StreamNodeResult` | 消失 | 消失 |

| 版本 | 范围 |
|------|------|
| v0.4 | ReAct = 有环图 + Agent 降维成 SubGraph + Context 驱动 + Overlay State + Control/Data Plane 分离 |
| v0.4+ | 砸碎 HashMap + Workflow<S> + Mutation 事件溯源 + 编译期 Merge |
| v0.5 | Multi-Agent Orchestration + Durable Execution + Agent↔Agent via MCP |
| v0.6 | Sampling |
