# LeLLM v0.4 架构演进

> 版本：v0.4 | 日期：2026-06-19 | 状态：规划中
>
> 本文档记录 v0.3 → v0.4 的设计决策和演进路线。

## 目录

- [一、v0.3 收尾：消灭双来源状态](#一v03-收尾消灭双来源状态)
- [二、v0.4 核心：ReAct = 有环图](#二v04-核心理act--有环图)
- [三、Control Plane / Data Plane 分离](#三control-plane--data-plane-分离)
- [四、NodeContext + StateStore](#四nodecontext--statestore)
- [五、StateStore 内建 ChangeLog](#五statestore-内建-changelog)
- [六、嵌套执行模型](#六嵌套执行模型)
- [七、v0.4+ 终局：Typed State + Effect 事件溯源](#七v04-终局typed-state--effect-事件溯源)
- [八、架构演进路线图](#八架构演进路线图)
- [九、关键设计决策](#九关键设计决策)

---

## 一、v0.3 收尾：消灭双来源状态

### 问题

当前 `ToolUseLoop` 持有私有 `LoopState`（`Vec<Message>`, `estimated_tokens`, `iterations` 等），
同时 Graph 层有自己的 `State = HashMap<String, Value>`。
**双来源状态 = Bug 温床。**

### 决策：方案 B+（统一状态来源）

`ToolUseLoop` 不再持有任何私有状态。所有 Agent 状态全部摊在 Graph State 中：

```rust
// 核心状态键收拢，成为底层图的公共契约
pub static SK_MESSAGES: StateKey<Vec<Message>> = StateKey::new("messages");
pub static SK_ITERATIONS: StateKey<u32> = StateKey::new("iterations");
pub static SK_TOOL_CALLS: StateKey<Vec<ToolCall>> = StateKey::new("tool_calls");
pub static SK_STOP_REASON: StateKey<StopReason> = StateKey::new("stop_reason");
pub static SK_OUTPUT_TOKENS: StateKey<usize> = StateKey::new("output_tokens");
pub static SK_REASONING_TOKENS: StateKey<usize> = StateKey::new("reasoning_tokens");
```

### 带来的质变

1. **单一事实来源（SSOT）**：外部系统通过 Checkpoint 观察 Agent 执行时，能清晰看到迭代轮次、Token 消耗等
2. **Agent 降维成 SubGraph**：`AgentFlowNode` 不再做复杂的内部编排，它自己就是由 LLM Node + Tool Node 组合的预制子图
3. **状态的确定性**：Checkpoint 保存的 State 与运行时看到的 State 完全一致

### 待做清单

- [ ] 从 `LoopState` 迁移所有字段到 Graph State keys
- [ ] `ToolUseLoop` 删除 `LoopState`，改为读写 Graph State
- [ ] `AgentFlowNode` 简化为 SubGraph 包装器
- [ ] 验证 Checkpoint 能正确恢复 Agent 中间状态

---

## 二、v0.4 核心：ReAct = 有环图

### 问题

当前 `ToolUseLoop` 是一个手写的 `while` 循环（`runtime.rs`）：
LLM 调用 → 检查 tool_calls → 执行工具 → 追加消息 → 回到 LLM。

### 决策：方案 B（中等粒度 Graph 建模）

```
[LLM] --有tool_calls--> [Tool] --(自环)--> [LLM]
     --无tool_calls--> [End]
```

### 为什么不选其他方案

| 方案 | 描述 | 评价 |
|------|------|------|
| A（粗粒度） | 整个 ReAct 封装成单个节点，用自环替代 while | 过于敷衍，失去 Graph 能力 |
| **B（中等粒度）** | **LLM Node + Tool Node + 条件边** | **可观测性与灵活性的平衡** |
| C（细粒度） | 每步独立节点（LLM → Condition → Tool → Compactor） | 过度拆分，ReAct 内部紧密耦合 |

### 与现有 ToolUseLoop 的关系

**方案 B — 直接替换：**
- `ToolUseLoop` 内部不再手写 `while` 循环
- 构建内部 Graph（LLM Node → Condition → Tool Node → 自环）
- 调用 `Graph::run_inline()` 驱动循环
- `ToolUseLoop` 变成一层薄壳，API 签名不变（用户无感知）

### 内部 ReAct Graph 的 State 传递

State 承载关键数据：

- `SK_MESSAGES` → 消息历史
- `SK_ITERATIONS` → 迭代计数
- `SK_TOOL_CALLS` → 本轮工具调用
- `SK_OUTPUT_TOKENS` → 累计输出 Token
- `SK_REASONING_TOKENS` → 累计推理 Token

### 嵌套结构

```
外部 Graph（用户编排）
  └── AgentFlowNode（Agent 适配为 Graph 节点）
        └── ReAct Engine（Graph Definition + run_inline）
              └── LLM ↔ Tool 循环
```

### 待做清单

- [ ] 设计 `LLMNode` — 执行单次 LLM 调用，写入 messages 和 tool_calls
- [ ] 设计 `ToolNode` — 读取 tool_calls，执行工具，写入 results
- [ ] 设计 `ConditionNode` — 检查 tool_calls 是否为空，路由到 ToolNode 或 End
- [ ] `Graph::run_inline()` 内联执行方法
- [ ] `ToolUseLoop` 内部构建 ReAct Graph，替代 while 循环
- [ ] 验证流式输出与现有 `AgentStream` 兼容

---

## 三、Control Plane / Data Plane 分离

### 问题

当前事件体系是三层包装的俄罗斯套娃：

```
ProviderEvent
    ↓
AgentEvent
    ↓
GraphEvent::Node { event: FlowEvent::Custom(...) }
```

Token 数据面事件（高频）与控制面事件（低频）共用同一通道，导致：
- 500 Token + 1 NodeCompleted 抢同一个 `mpsc::channel(32)`
- Token 事件撑爆通道，控制事件被延迟
- 架构污染 — 事件枚举越来越大

### 决策：拆成两个流

```rust
pub struct ExecutionHandle {
    /// 控制面 — 低频生命周期事件
    runtime_events: Receiver<RuntimeEvent>,
    /// 数据面 — 高频输出流
    output_stream: Receiver<StreamChunk>,
}
```

### RuntimeEvent（Control Plane）

```rust
pub enum RuntimeEvent {
    ExecutionStarted,
    NodeStarted,
    NodeCompleted,
    NodeFailed,
    BranchStarted,
    BranchCompleted,
    BarrierWaiting,
    BarrierReleased,
    CheckpointCreated,
    ExecutionCompleted,
}
```

**特点：** 低频、生命周期事件、拓扑事件、状态变化事件。

### StreamChunk（Data Plane）

```rust
pub enum StreamChunk {
    Text(String),
    Thinking(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}
```

**特点：** 高频、数据透传、无嵌套包装。

### AgentEvent 消失

不再定义 `AgentEvent`。所有 Agent 相关事件直接映射为 `StreamChunk`：

```
Provider Token      → StreamChunk::Text(...)
Provider Thinking   → StreamChunk::Thinking(...)
Tool 调用开始        → StreamChunk::ToolCall(...)
Tool 调用完成        → StreamChunk::ToolResult(...)
```

### 类比

| 领域 | Control Plane | Data Plane |
|------|--------------|------------|
| Kubernetes | API Server | Pod Traffic |
| MQTT | CONNECT / SUBSCRIBE | Message Payload |
| LeLLM v0.4 | RuntimeEvent | StreamChunk |

---

## 四、NodeContext + StateStore

### 问题

v0.3 的 `FlowContext` 承载了过多职责（State、事件发射器、TraceId 等），且 `StateDelta` 是节点外部概念，导致"节点驱动状态管理"的混乱。

### 决策：Runtime Handle 而非 Runtime State

```rust
pub struct NodeContext<'a> {
    /// 执行状态 — 借用引用，不拥有
    state: &'a mut StateStore,
    /// 数据面发射器 — 可选
    stream: Option<&'a StreamEmitter>,
}
```

**设计原则：**

- `NodeContext` 是 **Runtime Handle**（运行时句柄），不是 Runtime State
- 节点只借用，不拥有。零复制透传给子组件
- 只允许两类东西：State + Stream
- 禁止放入：RuntimeEventEmitter、TraceId、SpanId、GraphHandle、ExecutorConfig

### API 设计

```rust
pub trait FlowNode {
    async fn execute(
        &self,
        ctx: &mut NodeContext<'_>,
    ) -> Result<NodeOutput, GraphError>;
}

// 节点使用方式：
ctx.get(SK_MESSAGES)          // 读取
ctx.set(SK_ITERATIONS, n)     // 写入
ctx.append(SK_MESSAGES, msg)  // 追加
ctx.increment(SK_OUTPUT_TOKENS, tokens)  // 递增
ctx.emit(StreamChunk::Text(token))  // 发射数据面事件
```

### StreamEmitter 设计

```rust
pub struct StreamEmitter {
    tx: mpsc::Sender<StreamChunk>,
}
```

不直接暴露 `Sender`，未来可扩展 `emit_batch()`、`emit_throttled()`、`emit_if_subscribed()` 等。

---

## 五、StateStore 内建 ChangeLog

### 问题

v0.3 的 Delta 模型是"节点产生 Delta → Executor 收集 → apply"。
如果节点直接写 State，Delta 必须变成 StateStore 的内部实现。

### 决策：Event Sourcing Lite

```rust
pub struct StateStore {
    values: HashMap<KeyId, Value>,
    changes: Vec<ChangeRecord>,
}

pub struct ChangeRecord {
    key: StateKeyId,
    operation: ChangeOperation,  // Put | Append | Increment | Delete
    value: Value,
}

pub struct ChangeSet {
    changes: Vec<ChangeRecord>,
}
```

**StateDelta 改名 → ChangeRecord**，语义从"节点产生的增量"变为"状态系统记录的变更"。

### 节点写法不变

```rust
ctx.append(SK_MESSAGES, msg);    // 直接写，StateStore 内部自动记 ChangeRecord
ctx.increment(SK_OUTPUT_TOKENS, n);
ctx.set(SK_STOP_REASON, reason);
```

### 并行分支模型

```
base_state
   ↓ fork
branch_a (values + changelog)
branch_b (values + changelog)
   ↓
ReducerRegistry.merge(branch_a.changelog, branch_b.changelog)
   ↓
merged_changes → apply to base_state
```

**Reducer 的职责从"合并 Node Delta"变为"合并 Branch ChangeLog"。**

### Checkpoint 模型

```rust
Checkpoint {
    base_snapshot: State,
    recent_changes: ChangeSet,
}
```

恢复：`restore(snapshot) → apply(changes)` — 完全复用同一套机制。

### 架构对比

**v0.3（节点驱动状态管理）：**
```
FlowNode → StateDelta → Reducer → State
```

**v0.4（状态系统驱动状态管理）：**
```
FlowNode → NodeContext → StateStore → ChangeLog → Reducer → Checkpoint
```

---

## 六、嵌套执行模型

### 问题

AgentFlowNode 内部运行 ReAct Graph。如果内部 Graph 拥有自己的 Executor 和 RuntimeEvent 通道，会产生嵌套递归问题：

```
Execution
 └─ AgentNode
     └─ RuntimeEvent          ← 内部事件泄露
         └─ MCPTool
             └─ RuntimeEvent   ← 路径地狱
```

### 决策：内部不产生 RuntimeEvent

**核心原则：只有最外层拥有 Executor。**

```
Execution (唯一)
│
├── RuntimeEvent        ← 只属于最外层 Executor
│
└── StreamChunk         ← 可被任意嵌套组件透传
```

### Graph 的两种执行模式

| 模式 | 方法 | 场景 | Control Plane |
|------|------|------|--------------|
| 完整执行 | `Executor::execute()` | 用户 Graph | 产生 RuntimeEvent + StreamChunk |
| 内联执行 | `Graph::run_inline()` | AgentFlowNode 内部 ReAct | 仅 StreamChunk，无 RuntimeEvent |

### Graph::run_inline() 设计

```rust
impl Graph {
    /// 内联执行 — 不产生 RuntimeEvent，不 Checkpoint。
    /// 仅用于嵌套场景（如 AgentFlowNode 内部 ReAct 循环）。
    pub async fn run_inline(
        &self,
        ctx: &mut NodeContext<'_>,
        max_steps: usize,
    ) -> Result<(), GraphError>;
}
```

**`run_inline()` 只包含"路由解析 + 节点执行"的纯逻辑，剥离了：**
- RuntimeEvent 发射
- Checkpoint
- Barrier 等待
- Parallel 合并

### AgentFlowNode 实现

```rust
impl FlowNode for AgentFlowNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<NodeOutput, GraphError> {
        // 外部 Graph 已发出 NodeStarted("agent")
        let react_graph = self.build_react_graph();
        react_graph.run_inline(ctx, self.max_iterations).await?;
        // 外部 Graph 将发出 NodeCompleted("agent")
        Ok(NodeOutput { next: NextStep::GoToNext, .. })
    }
}
```

### 调试流

内部 ReAct 的调试信息不走 RuntimeEvent，单独引入：

```rust
pub enum AgentDebugEvent {
    IterationStarted { iteration: usize },
    IterationCompleted { iteration: usize },
    LLMRequest { iteration: usize },
    LLMResponse { iteration: usize },
    ToolSelected { tool_name: String },
    ToolFinished { tool_name: String },
}
```

仅在 `AgentDebugMode::Verbose` 开启时输出，默认用户看不到。

### 为什么这样设计

如果允许内部 Graph 拥有 RuntimeEvent：

1. 未来会出现 `RuntimeEvent::Nested(RuntimeEvent::Nested(...))` 的递归嵌套
2. 或者 `NodeStarted("agent/tool/planner/llm")` 的路径地狱
3. 三条嵌套 Agent 产生三个 RuntimeEvent 流，merge 逻辑爆炸

**禁止递归 RuntimeEvent = 架构防腐。**

---

## 七、v0.4+ 终局：Typed State + Effect 事件溯源

### 问题

v0.3 的 `HashMap<String, Value>` 是动态的、弱类型的。
`StateKey<T>` 和 `ReducerRegistry` 是补丁——在边界处做运行时类型检查。

### 终局愿景：Workflow<S> + Effect<S>

#### 核心 1：节点返回 Effect 而非 Delta

```rust
pub enum AgentEffect {
    AppendMessage(Message),
    IncrementIteration,
    RecordUsage(TokenUsage),
}

impl WorkflowState for AgentState {
    type Effect = AgentEffect;

    fn apply(&mut self, effect: Self::Effect) {
        match effect {
            AgentEffect::AppendMessage(msg) => self.messages.push(msg),
            AgentEffect::IncrementIteration => self.iterations += 1,
            AgentEffect::RecordUsage(usage) => self.usage += usage,
        }
    }
}
```

#### 核心 2：编译期 Merge 替代运行时 ReducerRegistry

```rust
pub trait Merge {
    fn merge(self, other: Self) -> Result<Self, WorkflowError>;
}

impl Merge for AgentState {
    fn merge(mut self, other: Self) -> Result<Self, WorkflowError> {
        self.messages.extend(other.messages);
        self.iterations = self.iterations.max(other.iterations);
        Ok(self)
    }
}
```

**零运行时字符串匹配开销。** 合并规则在编译期确定。

#### 核心 3：Checkpoint = Effect Log

- **持久化**：追加轻量级 Effect（如 `IncrementIteration`）到数据库，而非序列化几百 KB 的 JSON State
- **恢复**：重放 Effect Log，天然支持确定性重放测试（Deterministic Replay Testing）
- **可观测性**：每个 Effect 都是领域事件，天然可审计

---

## 八、架构演进路线图

```
  v0.3 (当前阶段: 大内聚/收拢)
  [消灭 LoopState] ──> [统一 StateKey (方案 B+)]
  [Agent 降维成 SubGraph] ──> [单一事实来源]
                                    │
                                    ▼
  v0.4 (破茧成蝶: 统一执行模型)
  [ReAct = 有环图] ──> [Control Plane / Data Plane 分离]
  [RuntimeEvent + StreamChunk] ──> [NodeContext + StateStore]
  [StateStore 内建 ChangeLog] ──> [Graph::run_inline() 内联执行]
  [AgentEvent 消失] ──> [Agent = Graph 的高级 DSL]
  [嵌套执行禁止递归 RuntimeEvent]
                                    │
                                    ▼
  v0.4+ (强类型领域)
  [砸碎 HashMap] ──> [Workflow<S>]
  [Effect 事件溯源] ──> [编译期 Merge]
                                    │
                                    ▼
  v0.5 (多智能体时代)
  [Multi-Agent Orchestration] ──> [Durable Execution]
  [Agent ↔ Agent via MCP] ──> [Sampling]
```

---

## 九、关键设计决策

| 决策 | 结论 | 理由 |
|------|------|------|
| v0.3 是否引入 TypedState | 否 | HashMap 骨架已铺设，v0.3 聚焦收拢 |
| LoopState 去留 | 消灭 | 双来源 = Bug 温床 |
| ReAct 建模粒度 | 中等（LLM + Tool + 条件边） | 可观测性与灵活性的平衡 |
| ToolUseLoop 替换方式 | 内部替换，API 不变 | 用户无感知迁移 |
| 事件体系 | RuntimeEvent + StreamChunk 分离 | Control Plane / Data Plane 解耦 |
| AgentEvent | 消失，合并为 StreamChunk | 消除俄罗斯套娃包装 |
| NodeContext | Runtime Handle，不拥有数据 | 借用语义，零复制透传 |
| StateDelta | 改名 ChangeRecord，内建到 StateStore | 状态系统驱动状态管理 |
| 嵌套执行 | 内部不产生 RuntimeEvent | 防止递归嵌套和路径地狱 |
| Graph 执行模式 | run_inline() + Executor::execute() | 区分内联与完整执行 |
| v0.4 TypedState 时机 | v0.4+ 专门 grill | 范围大，需要独立规划 |
| Effect vs Delta | v0.4+ 用 Effect 取代 Delta | 事件溯源 > 状态补丁 |
