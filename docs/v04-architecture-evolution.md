# LeLLM v0.4 架构演进

> 版本：v0.4 | 日期：2026-06-19 | 状态：规划中
>
> 本文档记录 v0.3 → v0.4 的设计决策和演进路线。

## 目录

- [一、v0.3 收尾：消灭双来源状态](#一v03-收尾消灭双来源状态)
- [二、v0.4 核心：ReAct = 有环图](#二v04-核心理act--有环图)
- [三、Control Plane / Data Plane 分离](#三control-plane--data-plane-分离)
- [四、NodeContext + StateStore](#四nodecontext--statestore)
- [五、Overlay State：StateSnapshot + BranchState](#五overlay-statestatesnapshot--branchstate)
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

- [x] 从 `LoopState` 迁移所有字段到 Graph State keys（已完成）
- [x] `ToolUseLoop` 删除 `LoopState`，改为读写 Graph State（已完成）
- [x] Agent 核心 StateKey 常量定义（已完成）
- [x] `AgentFlowNode` 支持 ReAct Graph 模式（已完成）
- [x] ReAct Graph 模式传播 StopReason 到外层 State（已完成）
- [ ] ReAct Graph 模式补全流式输出（StreamChunk emit）
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

### 内部 ReAct Graph 的结构

```
START → budget_check

budget_check --budget_ok--> [llm]
     --need_compact--> [compactor] → [llm]

[llm] → [tool_decision]
   --has_tool_calls--> [tool] → [budget_check] (循环)
   --no_tool_calls--> [end]
```

**关键设计：Compactor 是独立 FlowNode，LLMNode 不感知 Compaction。**

State 承载关键数据：

- `SK_MESSAGES` → 消息历史
- `SK_ITERATIONS` → 迭代计数
- `SK_TOTAL_TOOL_CALLS` → 累计工具调用
- `SK_OUTPUT_TOKENS` → 累计输出 Token
- `SK_REASONING_TOKENS` → 累计推理 Token
- `SK_COMPACT_COUNT` → 累计压缩次数
- `SK_STOP_REASON` → 停止原因

### 嵌套结构

```
外部 Graph（用户编排）
  └── AgentFlowNode（Agent 适配为 Graph 节点）
        └── ReAct Engine（Graph Definition + run_inline）
              └── LLM ↔ Tool 循环
```

### 待做清单

- [x] 设计 `LLMNode` — 执行单次 LLM 调用，写入 messages 和 tool_calls
- [x] 设计 `ToolNode` — 读取 tool_calls，执行工具，写入 results
- [x] 设计 `ReactCondition` — 检查 tool_calls 是否为空，路由到 ToolNode 或 End
- [x] 设计 `BudgetCondition` — 检查 Token 预算，路由到 Compactor 或 LLM
- [x] 设计 `CompactorNode` — 独立 FlowNode，职责单一（不感知 LLM）
- [x] `Graph::run_inline()` 内联执行方法
- [x] `ToolUseLoop` 内部构建 ReAct Graph，替代 while 循环
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

v0.3 的 `FlowNode::execute(&self, state: &State) -> Result<NodeOutput, GraphError>` 是**只读 State + Delta 输出**的模型。节点不直接写 State，而是输出 `Vec<StateDelta>`，由 Executor 统一 apply。

这导致：
1. ToolUseLoop 内部已经"偷偷"直接写 State（`state_push_assistant` 等辅助函数）
2. `NodeOutput` 和 `StreamNodeResult` 两套返回类型，`execute()` / `execute_stream()` 双路径维护
3. 节点返回了太多东西（deltas, next, metadata, observed, span_id）

### 决策：Context 驱动一切

统一原则 — 节点不返回业务数据，只返回 `Result<(), GraphError>`：

```
State      → ctx.set() / ctx.append() / ctx.increment()
Stream     → ctx.emit()
Metadata   → ctx.metadata().token_cost = 100.0
Control    → ctx.goto() / ctx.end() / ctx.pause()
```

```rust
pub struct NodeContext<'a> {
    /// 执行状态 — 直接写
    state: &'a mut BranchState,
    /// 数据面发射器 — 可选（阻塞模式 = None）
    stream: Option<&'a StreamEmitter>,
    /// 控制信号 — 节点写入，Executor 读取
    control: ExecutionControl,
    /// 节点元数据 — 节点写入
    metadata: NodeMetadata,
}
```

**`NodeContext` 是 Runtime Handle（运行时句柄），不是 Runtime State。**
- 节点只借用，不拥有。零复制透传给子组件
- 禁止放入：RuntimeEventEmitter、TraceId、SpanId、GraphHandle、ExecutorConfig

### FlowNode Trait — 统一单方法

```rust
pub trait FlowNode {
    async fn execute(
        &self,
        ctx: &mut NodeContext<'_>,
    ) -> Result<(), GraphError>;
}
```

**消失的中间类型：** `NodeOutput`、`StreamNodeResult`、`NodeMetadata`(返回值)

### State API — CRUD + Reducer 便捷方法

```rust
// 节点使用方式：
ctx.get::<Vec<Message>>(SK_MESSAGES)  // 读取，返回 clone
ctx.set(SK_ITERATIONS, n)             // 写入
ctx.append(SK_MESSAGES, msg)          // 追加
ctx.increment(SK_OUTPUT_TOKENS, n)    // 递增
ctx.remove::<T>(SK_KEY)               // 删除
ctx.emit(StreamChunk::Text(token))    // 发射数据面事件（无 stream 则静默丢弃）
```

### ExecutionControl — 控制信号

```rust
pub struct ExecutionControl {
    next: Option<NextStep>,            // None = 默认 Next
    signal: Option<ExecutionSignal>,   // None = 无信号
}

impl ExecutionControl {
    fn take(self) -> (NextStep, Option<ExecutionSignal>) {
        (self.next.unwrap_or(NextStep::Next), self.signal)
    }
}
```

```rust
pub enum NextStep {
    Next,         // 按拓扑顺序走下一步（默认值）
    Goto(NodeId), // 跳转到指定节点
    End,          // 结束执行
}

pub enum ExecutionSignal {
    Pause(BarrierWait),  // Barrier 挂起执行
}
```

```rust
// 节点控制流写法：
ctx.goto("tool_node");  // 跳转到工具节点
ctx.end();              // 结束执行
ctx.pause(BarrierWait { ... });  // Barrier 挂起
// 不调用任何控制方法 → 默认 NextStep::Next
```

**多次调用的语义：最后一次获胜。** 与 State 写入的"最后写入者胜"一致。

**Fallback 回归 Error Policy** — 节点不声明 Fallback。节点失败返回 `Err(GraphError)`，Executor 根据 `edge_fallback` 决定降级或终止。

### StreamEmitter 设计

```rust
pub struct StreamEmitter {
    tx: mpsc::Sender<StreamChunk>,
}
```

不直接暴露 `Sender`，未来可扩展 `emit_batch()`、`emit_throttled()`、`emit_if_subscribed()` 等。

---

## 五、Overlay State：StateSnapshot + BranchState

### 问题

v0.3 的 Delta 模型是"节点产生 Delta → Executor 收集 → apply"。
如果节点直接写 State，变更追踪必须变成 State 系统的内部实现。

同时，并行分支需要高效 fork — 不能深拷贝整个 State。

### 决策：拆成两个类型

不是用一个 `StateStore` 承担所有职责，而是按执行态拆分：

```rust
/// 不可变的状态快照 — 对应全量 Checkpoint
pub struct StateSnapshot {
    values: HashMap<KeyId, Value>,
}

/// 可写的分支状态 — 一层 Overlay，对应增量 Checkpoint
pub struct BranchState {
    base: Arc<StateSnapshot>,          // fork = O(1)
    local: HashMap<KeyId, Value>,      // 本层写入缓存
    changes: Vec<ChangeRecord>,        // 变更日志
}
```

**Overlay 模型的核心约束：永远只有一层 overlay，不是 MVCC 链。**

### 读取 — O(1)

```rust
fn get(&self, key: KeyId) -> Option<&Value> {
    self.local.get(&key)
        .or_else(|| self.base.values.get(&key))
}
```

最多查两层。不存在递归风险。

### 写入 — 自动记 ChangeRecord

```rust
fn set(&mut self, key: KeyId, value: Value) {
    self.local.insert(key, value);
    self.changes.push(ChangeRecord {
        key,
        operation: ChangeOperation::Put,
        value,
    });
}
```

**ChangeLog 生命周期：节点级别。** 每次 `FlowNode::execute()` 前清空 changes，执行后 Executor 收集。

**同 key 多次操作：不合并。** 每次操作产生一条 ChangeRecord。忠实记录，便于审计。

### Fork — O(1)

```rust
fn fork(&self) -> BranchState {
    BranchState {
        base: Arc::new(self.to_snapshot()),  // apply changes → new snapshot
        local: HashMap::new(),
        changes: Vec::new(),
    }
}
```

ParallelNode fork 时：`apply_changes(base, changes)` 生成新 snapshot，各分支独立 Overlay。

### Merge — O(branches × changes)

```
branch_a.changes
branch_b.changes
branch_c.changes
   ↓
ReducerRegistry.merge(所有 changes)
   ↓
apply(snapshot, merged_changes)
   ↓
new_snapshot
```

**Reducer 的职责从"合并 Node Delta"变为"合并 Branch ChangeLog"。**

### Checkpoint 模型

```rust
Checkpoint {
    base_snapshot: StateSnapshot,   // 全量
    recent_changes: ChangeSet,      // 增量
}
```

- `StateSnapshot` 天然对应全量 Checkpoint
- `changes` 天然对应增量 Checkpoint
- 恢复：`restore(snapshot) → apply(changes)` — 完全复用同一套机制

### 架构对比

**v0.3（节点驱动状态管理）：**
```
FlowNode → StateDelta → Reducer → State
```

**v0.4（Context 驱动，状态系统自动追踪）：**
```
FlowNode → NodeContext → BranchState → ChangeLog → Reducer → Checkpoint
```

| 维度 | v0.3 Delta 模型 | v0.4 Overlay 模型 |
|------|----------------|------------------|
| 节点写法 | 返回 `Vec<StateDelta>` | `ctx.set()` / `ctx.append()` |
| 变更追踪 | 节点显式产生 | State 系统自动记录 |
| 并行 Fork | `state.clone()`（深拷贝） | `base.to_snapshot()`（O(changes)） |
| 并行 Merge | `merge_deltas()` | `merge_changes()` |
| 中间类型 | `NodeOutput`, `StreamNodeResult` | 消失 |

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
- Barrier 等待（内联模式不支持 Pause 信号）
- Parallel 合并

内联模式：`NodeContext` 的 `stream = None`，节点照常 `execute()`，事件静默丢弃。

**`NextStep::Next` 在内联模式下的处理：支持边条件路由。** 与外部 `Executor` 行为完全一致——节点返回 `NextAction::Next` 时，`run_inline()` 调用 `resolve_next_inline()` 解析边条件（条件边 → 普通边 → Fallback 边）。这样内外行为统一，减少认知负担。开销可忽略（内联 Graph 节点数极少）。

### AgentFlowNode 实现

```rust
impl FlowNode for AgentFlowNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        // 外部 Graph 已发出 NodeStarted("agent")
        let react_graph = self.build_react_graph();
        react_graph.run_inline(ctx, self.max_iterations).await?;
        // 外部 Graph 将发出 NodeCompleted("agent")
        Ok(())
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

#### 核心 1：节点返回 Effect 而非 Delta — ✅ 已实现

```rust
pub enum AgentEffect {
    AppendMessage(Message),
    AppendMessages(Vec<Message>),
    IncrementIteration,
    AddToolCalls(usize),
    AddOutputTokens(usize),
    AddReasoningTokens(usize),
    IncrementCompactCount,
    ReplaceMessages(Vec<Message>),
    SetStopReason(StopReason),
    SetLastResponse(ChatResponse),
}

impl WorkflowState for AgentState {
    type Effect = AgentEffect;

    fn apply(&mut self, effect: Self::Effect) {
        match effect {
            AgentEffect::AppendMessage(msg) => self.messages.push(msg),
            AgentEffect::IncrementIteration => self.iterations += 1,
            // ...
        }
    }
}
```

#### 核心 2：编译期 Merge 替代运行时 ReducerRegistry — ✅ 已实现

```rust
impl WorkflowState for AgentState {
    fn merge(self, other: Self) -> Result<Self, WorkflowError> {
        Ok(Self {
            messages: self.messages.into_iter().chain(other.messages).collect(),
            iterations: self.iterations.max(other.iterations),
            total_tool_calls: self.total_tool_calls.max(other.total_tool_calls),
            output_tokens: self.output_tokens + other.output_tokens,
            reasoning_tokens: self.reasoning_tokens + other.reasoning_tokens,
            // ...
        })
    }
}
```

**零运行时字符串匹配开销。** 合并规则在编译期确定。

#### 核心 3：Checkpoint = Effect Log — 📋 规划中

- **持久化**：追加轻量级 Effect（如 `IncrementIteration`）到数据库，而非序列化几百 KB 的 JSON State
- **恢复**：重放 Effect Log，天然支持确定性重放测试（Deterministic Replay Testing）
- **可观测性**：每个 Effect 都是领域事件，天然可审计

### 已完成的工作（2026-06-20）

- [x] `WorkflowState` trait + `Effect` trait（`lellm-graph/src/workflow_state.rs`）
- [x] `NodeContext` 添加 effects 缓冲（`emit_effect` / `consume_effects`）
- [x] `NodeContext` 添加 typed 访问（`get_state` / `set_state`）
- [x] `AgentState` struct + `AgentEffect` enum（`lellm-agent/src/runtime/typed_state.rs`）
- [x] ReAct 节点全面重构 — 消除 `create_state_from_ctx` / `sync_state_to_ctx`
- [x] `ToolUseLoop::execute` 使用 Typed State 初始化 + 结果提取
- [x] `AgentFlowNode::execute_with_react_graph` 使用 Typed State
- [x] `StopReason` 加 serde derive（支持序列化）
- [ ] Effect Log 持久化到 Checkpoint
- [ ] 确定性重放测试

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
| FlowNode 签名 | `execute(ctx) -> Result<(), GraphError>` | Context 驱动一切，零歧义 |
| NodeOutput | 消失 | 所有数据写入 Context |
| StreamNodeResult | 消失 | 统一为单方法 |
| NextStep | 仅保留路由语义（Next, Goto, End） | 不混入控制信号 |
| ExecutionSignal | 独立枚举（Pause） | Barrier 挂起不是路由 |
| Fallback | 回归 Error Policy | 节点不声明 fallback，边定义驱动 |
| State 模型 | StateSnapshot + BranchState 双层 Overlay | Fork O(1)，Merge O(changes) |
| ChangeLog | 节点级别，不合并 | 忠实记录，便于审计 |
| 嵌套执行 | 内部不产生 RuntimeEvent | 防止递归嵌套和路径地狱 |
| Graph 执行模式 | run_inline() + Executor::execute() | 区分内联与完整执行 |
| v0.4 TypedState 时机 | v0.4+ 专门 grill | 范围大，需要独立规划 |
| Effect vs Delta | v0.4+ 用 Effect 取代 Delta | 事件溯源 > 状态补丁 |
