# LeLLM v0.4 架构演进

> 版本：v0.4 | 日期：2026-06-29 | 状态：实施中（约 55-60% 完成）
>
> 本文档记录 v0.3 → v0.4 的设计决策和演进路线。
>
> **2026-06-29 更新：** 基于代码实际状态全面审计，修正了过时的"待完成"标记。
> 详见 [Plan vs Reality 对照表](../discuss/v04-plan-vs-reality.md)。

## 目录

- [一、v0.3 收尾：消灭双来源状态](#一v03-收尾消灭双来源状态)
- [二、v0.4 核心：ReAct = 有环图](#二v04-核心理act--有环图)
- [三、Control Plane / Data Plane 分离](#三control-plane--data-plane-分离)
- [四、NodeContext + StateStore](#四nodecontext--statestore)
- [五、Overlay State：StateSnapshot + BranchState](#五overlay-statestatesnapshot--branchstate)
- [六、嵌套执行模型](#六嵌套执行模型)
- [七、v0.4+ 终局：Typed State + Mutation 事件溯源](#七v04-终局typed-state--mutation-事件溯源)
- [七补充、Mutation Only 架构决策（2026-06-21 Grill Session）](#七补充mutation-only-架构决策2026-06-21-grill-session-确认)
- [八、架构演进路线图](#八架构演进路线图)
- [九、关键设计决策](#九关键设计决策)
- [十、ADR 归档（2026-06-25 ~ 2026-06-29）](#十adr-归档2026-06-25--2026-06-29)

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
- [x] ReAct Graph 模式补全流式输出（StreamChunk emit）（已完成）
- [ ] 验证 Checkpoint 能正确恢复 Agent 中间状态（Checkpoint 集成未完成）

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
- [x] 验证流式输出与现有 `AgentStream` 兼容（已完成）

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

## 七、v0.4+ 终局：Typed State + Mutation 事件溯源

### 问题

v0.3 的 `HashMap<String, Value>` 是动态的、弱类型的。
`StateKey<T>` 和 `ReducerRegistry` 是补丁——在边界处做运行时类型检查。

### 终局愿景：Workflow<S> + Mutation<S>

#### 核心 1：节点返回 Mutation 而非 Delta — ✅ 已实现

```rust
pub enum AgentMutation {
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
    type Mutation = AgentMutation;

    fn apply(&mut self, mutation: Self::Mutation) {
        match mutation {
            AgentMutation::AppendMessage(msg) => self.messages.push(msg),
            AgentMutation::IncrementIteration => self.iterations += 1,
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

#### 核心 3：Checkpoint = Snapshot（非 Mutation Log）— ✅ 架构完成，❌ 未集成

> **2026-06-29 修正：** Checkpoint 采用 Snapshot 模式（非 Mutation Replay），
> 因为"给我一个 Checkpoint 就能恢复"比 Mutation Replay 更简单可靠。

**已完成的架构：**

```
Checkpoint<S> (typed snapshot)
  → CheckpointCodec<S> (serialize/deserialize)
    → CheckpointBlob (bytes + metadata)
      → BlobCheckpointStore (bytes in/out SPI)
        → InMemoryBlobStore (HashMap backend)
```

**未完成的工作：**

- execution_loop 未接入 Checkpoint 保存
- 恢复路径（load checkpoint → restore state → resume from node）不存在
- Barrier 恢复（decision queue 重建）未设计

### 已完成的工作（2026-06-20）

- [x] `WorkflowState` trait + `Mutation` trait（`lellm-graph/src/workflow_state.rs`）
- [x] `NodeContext` 添加 mutations 缓冲（`emit_mutation` / `consume_mutations`）
- [x] `NodeContext` 添加 typed 访问（`get_state` / `set_state`）
- [x] `AgentState` struct + `AgentMutation` enum（`lellm-agent/src/runtime/typed_state.rs`）
- [x] ReAct 节点全面重构 — 消除 `create_state_from_ctx` / `sync_state_to_ctx`
- [x] `ToolUseLoop::execute` 使用 Typed State 初始化 + 结果提取
- [x] `AgentFlowNode::execute_with_react_graph` 使用 Typed State
- [x] `StopReason` 加 serde derive（支持序列化）

### v0.4+ 待做清单（2026-06-29 状态更新）

**已完成：**

- [x] 消灭 dual-write（`emit_and_set` 消失），Mutation 成为唯一状态变更来源
- [x] `NodeContext.mutations` 改为 `Vec<S::Mutation>`（强类型）
- [x] `Graph<S>` 完全泛型化 + `NodeKind<S>` + `Edge<S>`
- [x] `NodeContext<S>` 一刀切 — LeafContext 无 HashMap API，NodeContext 保留 replace_state
- [x] `FlowNode<S>` 泛型化 + 全链适配（向后兼容）
- [x] `ExecutionEngine<S>` 即时 apply Effects（commit()）
- [x] `ParallelNode<S>` merge + `replace_state()`（ExecutorOperation）
- [x] `AgentState` + `AgentMutation` 桥接（无 StateExtractor trait，直接用 AgentState）
- [x] LlmInvoker（Retry + Fallback + Stream State Machine）
- [x] ExecutionTrace 类型定义（TraceStep, TraceSink, ExportedTrace）
- [x] Checkpoint 分层架构（Codec + Blob + BlobStore）

**未完成：**

- [ ] `ConditionNode` → `LeafNode` 迁移
- [ ] `BarrierNode` → `LeafNode` 迁移
- [ ] `TaskNode` → 考虑新增 `LeafTaskNode`
- [ ] Checkpoint 集成到 execution_loop（保存 + 恢复）
- [ ] ExecutionTrace 集成到 execution_loop
- [ ] `run_inline_stream()` API（或确认不需要）
- [ ] Message Store（Mutation 存 message_id，本体走外部存储）
- [ ] Mutation Log Checkpoint（替代 State Snapshot）— 待 v0.5 决策
- [ ] 确定性重放测试
- [ ] 文件拆分：`node_context.rs` (575 行) → `execution_engine.rs` + `node_context.rs`
- [ ] 文件拆分：`execution_loop.rs` (469 行) → 拆出 `barrier_wait.rs`

---

## 七补充、Mutation Only 架构决策（2026-06-21 Grill Session 确认）

### 决策总览

| # | 决策 | 结论 |
|---|------|------|
| 1 | Mutation Only | 消除 dual-write，Mutation 是唯一状态变更来源 |
| 2 | Executor 即时 apply | 节点纯函数（读 State → 产 Mutation），Executor 消费 + apply |
| 3 | Graph<S> 完全泛型 | 每个 Graph 有自己的 `S: WorkflowState` |
| 4 | NodeContext<S> 一刀切 | 删除 HashMap API，只保留 `state()` + `emit_mutation()` |
| 5 | 纯 Mutation | 节点不直接写 State。ParallelNode 例外：`replace_state()` 用于 merge |
| 6 | StateExtractor trait | AgentFlowNode 桥接外部 State ↔ AgentState |
| 7 | Checkpoint = Snapshot | 存完整 State Snapshot（非 Mutation Replay）。"给我一个 Checkpoint 就能恢复"。架构完成，集成未开始 |
| 8 | Message | Mutation 存完整 Message（非 message_id 引用）。简单可靠，无需 Message Store |

### 目标架构（2026-06-29 修正）

```
Graph<S: WorkflowState>
  ├─ Edge<S>              — 条件闭包: &S -> bool
  ├─ NodeKind<S, M>
  │    ├─ External(Arc<dyn FlowNode<S>>)     — 向后兼容
  │    ├─ ExternalLeaf(Arc<dyn LeafNode<S>>) — 声明式（推荐）
  │    ├─ Task(TaskNode<S>)                   — FlowNode（待迁移）
  │    ├─ Condition(ConditionNode<S>)         — FlowNode（待迁移）
  │    ├─ Barrier(BarrierNode<S>)             — FlowNode（待迁移）
  │    └─ Parallel(ParallelNode<S, M>)        — ExecutorOperation（已完成）
  │
  ├─ LeafContext<'a, S>     — 只读 &S + record(Mutation) + emit(StreamChunk)
  ├─ NodeContext<'a, S>     — 可变 &mut S + record + emit + replace_state（向后兼容）
  │
  └─ ExecutionEngine<S>
       └─ commit(): take_mutations → state.apply_batch()

AgentFlowNode
  └─ 内部 Graph<AgentState, AgentStateMerge> + run_inline(&mut ExecutionEngine)

Checkpoint<S> (Snapshot, 非 Mutation Log)
  └─ CheckpointCodec → CheckpointBlob → BlobCheckpointStore

ExecutionTrace<E> (审计, 未集成)
  └─ Vec<TraceStep<E>> — step, node_id, mutations
```

### 核心变更

#### 1. 消灭 Dual-Write

**之前（双写反模式）：**
```rust
fn emit_and_set(ctx: &mut NodeContext<'_>, mutation: AgentMutation) {
    ctx.emit_mutation(mutation.clone());  // ← 路径 A: Mutation 驱动
    match mutation {
        AgentMutation::IncrementIteration => {
            let cur = ctx.get(SK_ITERATIONS);
            ctx.set(SK_ITERATIONS, cur + 1);  // ← 路径 B: HashMap 直接写
        }
        // ... 每个 variant 都要 dual-write
    }
}
```

**之后（Mutation Only）：**
```rust
// 节点只 emit Mutation
ctx.emit_mutation(AgentMutation::IncrementIteration);

// Executor 循环中即时 apply
let mutations = ctx.consume_mutations();
state.apply_batch(mutations);
```

#### 2. Graph<S> 完全泛型

**之前：**
```rust
pub type EdgeCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;
//                              ^^^^^^ 固定为 HashMap<String, Value>
```

**之后：**
```rust
pub type EdgeCondition<S> = Arc<dyn Fn(&S) -> bool + Send + Sync>;
pub struct Graph<S: WorkflowState> { ... }
pub struct NodeKind<S: WorkflowState> { ... }
pub trait FlowNode<S: WorkflowState> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError>;
}
```

#### 3. NodeContext<S> 一刀切

**之前：**
```rust
// HashMap API
ctx.get::<Vec<Message>>("messages")  // 运行时类型检查
ctx.set("iterations", n)             // 字符串 key
ctx.append("messages", msg)          // 动态数组
```

**之后：**
```rust
// 强类型 API
ctx.state().messages.clone()         // 编译期类型安全
ctx.emit_mutation(IncrementIteration)  // 语义清晰
```

#### 4. StateExtractor 桥接

```rust
pub trait StateExtractor<S: WorkflowState>: Send + Sync {
    fn extract_messages(&self, state: &S) -> Vec<Message>;
    fn inject_result(&self, state: &mut S, agent_state: &AgentState);
}

pub struct AgentFlowNode<S, E> {
    extractor: E,
    // ... Agent 配置
}

impl<S: WorkflowState, E: StateExtractor<S>> FlowNode<S> for AgentFlowNode<S, E> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        let messages = self.extractor.extract_messages(ctx.state());
        let mut inner_state = AgentState::from_messages(messages);
        
        let react_graph = self.build_react_graph();
        react_graph.run_inline(&mut inner_state, self.max_iterations).await?;
        
        self.extractor.inject_result(ctx.state(), &inner_state);
        Ok(())
    }
}
```

#### 5. Checkpoint = Snapshot（非 Mutation Log）

> **2026-06-29 修正：** 采用 Snapshot 模式，非 Mutation Replay。

**之前：** 序列化整个 `State = HashMap<String, Value>`
**之后：** 序列化 `Checkpoint<S>`（强类型），通过 `CheckpointCodec` 转换为 `CheckpointBlob`。

```rust
// 存储
Checkpoint<S> {
    checkpoint_id: CheckpointId,
    current_node: NodeId,
    state: S,                      // 完整 Snapshot
    created_at: SystemTime,
}
  → CheckpointCodec<S> → CheckpointBlob { bytes, graph_hash, codec, metadata }
    → BlobCheckpointStore → InMemoryBlobStore / SQLite / S3

// 恢复
let blob = store.load_latest(trace_id).await?;
let checkpoint = codec.deserialize(&blob)?;
resume_from(checkpoint.current_node, checkpoint.state);
```

**Mutation Log Checkpoint 推迟到 v0.5。** 原因：
- Snapshot 更简单可靠（"给我一个 Checkpoint 就能恢复"）
- Mutation Replay 需要确定性保证（工具调用、LLM 响应）
- Message Store 引用增加了复杂度
- v0.4 先验证 Snapshot 模式是否足够
let blob = store.load_latest(trace_id).await?;
let checkpoint = codec.deserialize(&blob)?;
resume_from(checkpoint.current_node, checkpoint.state);
```

**Barrier 恢复（未设计）：**
- decision queue 需要重建
- 如果恢复点在 Barrier 节点，需要重新等待或跳过
```

#### 6. Message — 存完整 Message（非引用）

> **2026-06-29 修正：** Mutation 存完整 `Message`，不引入 Message Store。

`AppendMessage(Message)` 存完整 `Message`。

**理由：**
- 简单可靠——Checkpoint 自包含，不依赖外部 Store
- Message 体积可控（Agent 有 Compaction 机制）
- Message Store 增加复杂度（一致性、TTL、GC）
- v0.4 先验证 Snapshot 模式，v0.5 再评估是否需要 Message Store

---

## 八、架构演进路线图

```
  v0.3 (已完成: 大内聚/收拢)
  [消灭 LoopState ✅] ──> [统一 StateKey ✅]
  [Agent 降维成 SubGraph ✅] ──> [单一事实来源 ✅]
                                    │
                                    ▼
  v0.4 (进行中: 统一执行模型，约 55-60%)
  [ReAct = 有环图 ✅] ──> [Control Plane / Data Plane 分离 ✅]
  [RuntimeEvent + StreamChunk ✅] ──> [ExecutionEngine + LeafContext ✅]
  [Typed WorkflowState ✅] ──> [Graph::run_inline() ✅]
  [AgentEvent 保留（适配层）] ──> [Agent = Graph 的高级 DSL ✅]
  [LeafNode ✅ + FlowNode(向后兼容) ⚠️]
  [ExecutorOperation ✅ (ParallelNode)]
  [LlmInvoker ✅ (Retry + Fallback + Stream SM)]
  [Checkpoint 架构 ✅，集成 ❌]
  [ExecutionTrace 类型 ✅，集成 ❌]
                                    │
                                    ▼
  v0.4 收尾（待完成）
  [ConditionNode → LeafNode]
  [BarrierNode → LeafNode]
  [Checkpoint 集成到 execution_loop]
  [ExecutionTrace 集成到 execution_loop]
  [文件拆分：node_context.rs, execution_loop.rs]
                                    │
                                    ▼
  v0.5 (多智能体时代)
  [Multi-Agent Orchestration] ──> [Durable Execution]
  [Agent ↔ Agent via MCP] ──> [Mutation Log Checkpoint?]
  [Scheduler] ──> [Pause / Resume]
  [ExecutorOperation 全量迁移]
  [Graph::run_inline → Engine.run()]
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
| Graph 执行模式 | run_inline() + run_execution_loop() | 区分内联与完整执行 |
| v0.4 TypedState 时机 | v0.4 已实现 | WorkflowState trait + AgentMutation |
| Mutation vs Delta | v0.4 用 Mutation 取代 Delta | 已完成 |
| Checkpoint 模式 | Snapshot（非 Mutation Replay）| 简单可靠，v0.5 再评估 Replay |
| Message 存储 | Mutation 存完整 Message | 不引入 Message Store |
| AgentEvent | 保留（AgentEventSink 适配 StreamChunk）| 非消失，是合理的适配层 |
| FlowNode 去留 | 保留（向后兼容），标记 deprecated | 新代码用 LeafNode |
| spawn_child | 不存在，ParallelNode 直接 ExecutionEngine::new | 不需要抽象 |

---

## 十、ADR 归档（2026-06-25 ~ 2026-06-29）

> 本节汇总了 v0.4 实施过程中产生的 4 份架构决策记录（ADR），原文档已合并至此。

---

### ADR-0001：StreamSink 抽象 — Producer Push 模型

**日期：** 2026-06-25 | **状态：** Accepted

#### 上下文

Graph 层需要支持流式执行。初始设计让 `Graph::execute_stream()` 返回 `mpsc::Receiver<T>`，将 Tokio channel 泄漏到 Graph 层。

#### 决策

**StreamSink Trait** — 同步 `emit`，Producer Push，Graph 不感知传输机制：

```rust
pub trait StreamSink: Send + Sync {
    fn emit(&self, chunk: StreamChunk);
}
```

**BufferedSink + Forward Task** — Node → BufferedSink（O(1)）→ Forward Task（异步消费）→ Consumer（backpressure 在此层处理）。

**取消 = 消费者离开**（不是背压）。消费者断开 → receiver drop → forward task exit → CancellationToken.cancel() → 所有 Node 停止。

**Step Boundary Commit** — Token 流式只走 Stream（emit），不写 State。流结束 → 一次性 `emit_mutation(AppendMessage(...))`。

**StreamChunk — Execution View**（展示内容，不是 Message）：

```rust
pub enum StreamChunk {
    TextDelta(String),
    ThinkingDelta(String),
    ToolLifecycle { phase: ToolPhase, call_id: String, tool_name: String },
    ToolOutput { call_id: String, tool_name: String, content: String, is_error: bool },
}
```

State Plane（`Message::ToolResult`，完整）与 Data Plane（`ToolOutput`，展示用）永不互相引用。

**Tool 并发 emit 协议** — Start 保证顺序（A, B, C），End 允许乱序（按实际完成顺序），通过 call_id 关联。

**Graph API 统一** — `run_inline_stream(state, sink)` 统一流式与阻塞。sink=None 等价于阻塞。

**Agent API 统一** — `ToolUseLoop::execute_stream()` 统一流式与阻塞。提供 `ChannelSink` 包装。

#### 后果

正面：Graph 层完全解耦传输机制；Node 执行成本固定；取消传播立即主动；StreamChunk 成为一等协议。
负面：BufferedSink 在极端情况下可能占用内存；`execute()` 删除是破坏性变更。

---

### ADR-0002：统一执行路径 + LlmInvoker 分层

**日期：** 2026-06-28 | **状态：** Accepted

#### 上下文

`ToolUseLoop` 存在两条执行路径：`execute()` 走 Graph + react graph（~60 行），`execute_stream()` 手写 while 循环（~250 行）绕过 Graph。每次修改 Agent 循环逻辑必须在两个地方应用。

同时，`lellm-graph/src/hook.rs`（229 行）是死代码——executor 从未调用，且与 agent 层 `AgentHook` 同名冲突。

#### 决策

**1. 删除 Graph 层 hook.rs** — 消除 `AgentHook` 同名冲突，修复 graph → agent 概念泄漏。

**2. StreamSink 是唯一的消费抽象** — 没有 `StreamAdapter`、没有 `on_finish()`（Rust 的 `Drop` + channel close 就是 finish）。

**3. 统一执行路径**：

```
AgentRuntime → Graph::run_inline_stream(state, sink) → StreamSink → Agent API
```

`execute_stream()` 不再包含任何业务逻辑，只负责创建 channel + sink + 调用 graph。

**4. LlmInvoker 分层**：

```
ReAct Graph → LLMNode → LlmInvoker → LlmProvider → HTTP Client
```

LlmInvoker 负责 retry、fallback、circuit breaker、stream state machine、metrics。
LlmProvider 保持 stateless protocol adapter。
不做 ToolInvoker — 工具不需要 Invoker 层。

**Stream State Machine** 决定 retry 边界：`NotStarted`（retry OK）→ `HeadersReceived`（retry OK）→ `FirstChunkSent`（abort）→ `Finished`。

#### 实施顺序

1. 创建 `AgentEventSink`（实现 `StreamSink`）
2. 创建 `LlmInvoker`（包装 `InvocationPlan`）
3. 改造 `LLMNode`（接收 `Arc<LlmInvoker>`）
4. 改造 `execute_stream()`（删掉手写 while 循环）
5. 删除 `iteration.rs` 中流式专用代码
6. 清理被 typed state 替代的 State 辅助函数

#### 后果

正面：Agent 循环逻辑集中在 react.rs；一处修 bug 两条路径受益；删除 ~500 行重复/死代码。
负面：AgentEventSink 需要完整覆盖转换逻辑；LlmInvoker 是新组件需要充分测试。

---

### ADR-0003：LeafContext / ExecutorOperation 执行模型分裂

**日期：** 2026-06-29 | **状态：** Accepted

#### 背景

v0.4 引入了 `ExecutionEngine` + `ExecutorState` 统一执行模型。在此基础上，进一步细化节点的能力边界：Leaf 节点只需读 State + emit Mutation，Composite 节点需要 clone/merge/replace_state 等完整能力。

#### 决策

**1. LeafContext — 纯借用视图**：`state` 字段为 `&S`（只读），不提供 `replace_state()`，编译期保证 Leaf 节点无法修改 State。

**2. LeafNode trait** — 接收 `LeafContext`（只读），语义上表达"此节点只做声明式业务逻辑"。

**3. NodeKind 新增 ExternalLeaf 变体** — 三个执行循环统一 match dispatch：`External` → `build_node_context()` → `FlowNode`；`ExternalLeaf` → `build_leaf_context()` → `LeafNode`。

**4. ExecutorOperation 保留给 Composite 节点** — 直接接收 `&mut ExecutionEngine`，拥有完整能力（clone/merge/replace_state/spawn_child）。

#### 职责边界

```
Graph (AST) → NodeKind (不实现任何执行 trait)

ExecutionEngine (runtime owner)
    ├── dispatch → match NodeKind
    ├── build_leaf_context() → LeafNode
    ├── build_node_context() → FlowNode (backward compat)
    └── pass &mut self → ExecutorOperation

LeafNode → 只能 emit Mutation (LLM, Tool, Guard, Compactor)
ExecutorOperation → 可以操纵 Executor (Parallel, Retry, Loop, SubGraph)
```

#### 已迁移节点

LLMNode, ToolNode, PostLLMGuard, CompactorNode, BudgetCondition — 全部从 `FlowNode` 迁移为 `LeafNode`。

#### 影响

正面：编译期安全（Leaf 无法修改 State）；意图清晰；零运行时开销；渐进式迁移。
负面：API 表面积增加；需要理解 Leaf vs Composite vs ExecutorOperation 的区别。

---

### v0.4 执行模型重构 — 设计决策总览

**日期：** 2026-06-29 | **状态：** Phase A/B/D/E 完成，Phase C 部分完成

#### 决策总览

| # | 决策 | 状态 | 说明 |
|---|------|------|------|
| 1 | 删除 GraphExecutor | ✅ | executor.rs 已删除 |
| 2 | 删除 BranchState | ✅ | branch_state.rs 已删除 |
| 3 | 删除 delta.rs + ReducerRegistry | ✅ | delta.rs 已删除 |
| 4 | Checkpoint 分层架构 | ✅ | 5 层解耦架构，9 个新测试 |
| 5 | Checkpoint 集成 | ❌ | execution_loop 未接入 save_checkpoint |
| 6 | ExecutionEngine + ExecutorState | ✅ | ExecutionContext 为 type alias |
| 7 | Executable 统一抽象 | ⚠️ | `emit_flow_event` 已加入；`Executable` trait 因 dyn compatibility 限制放弃 |
| 8 | NodeContext 瘦身 | ✅ | 已删除 branch 字段 |
| 9 | LeafNode + LeafContext | ✅ | ReAct 节点全部迁移 |
| 10 | ExecutorOperation | ✅ | ParallelNode 已实现 |
| 11 | LlmInvoker | ✅ | Retry + Fallback + Stream State Machine |
| 12 | ExecutionTrace | ⚠️ | 类型已定义，execution_loop 未接入 |
| 13 | 测试迁移 | ✅ | SimpleExecutor 兼容层 + execution_loop 独立模块 |

**总删除量：** ~1700+ 行（executor.rs ~1170 + branch_state.rs ~180 + delta.rs ~340）

#### Checkpoint 分层架构

```
Checkpoint<S> → CheckpointCodec<S> → CheckpointBlob → BlobCheckpointStore → InMemoryBlobStore
```

新增类型：`CheckpointBlob`、`CheckpointCodec<S>`、`SerdeCheckpointCodec<S>`、`BlobCheckpointStore`、`InMemoryBlobStore`、`TypedCheckpointStore`、`CheckpointStoreError::Serialization`。

#### Executable 统一抽象 — 部分放弃

`Executable<S>` + `dyn ExecutorState<S>` 组合不可行，因为：
1. `build_node_context()` 返回生命周期绑定的引用 — Rust 不允许 dyn trait 方法返回与自身生命周期绑定的引用
2. `apply_batch()` 使用 `impl IntoIterator` — 泛型方法破坏 dyn compatibility

**结论：** 保持 `FlowNode<S>` 作为主要 trait，`ExecutorState<S>` 用于静态分发（Composite 节点内部使用）。

#### 已知问题

1. `state_mut_ref()` Hack — ParallelNode 合并子分支时需要修改父 state，已替换为 `replace_state()`
2. `GraphResult` 硬编码 `State` — v0.5 重构时泛型化
3. `BarrierNode` StateMutation 约束 — v0.5 待决策
4. `FlowEvent::Custom` Box<dyn Any> — 低优先级，未来需要持久化时重新设计
5. `ExecutorState<S>` 不是 dyn compatible — `build_node_context()` 返回生命周期引用 + `apply_batch()` 用 `impl IntoIterator`
6. `spawn_child()` 只存在于注释 — ParallelNode 直接 `ExecutionEngine::new()` 创建子 engine

#### 后续清理项

- [ ] 迁移 `BarrierNode` → `LeafNode`（只读 state + pause 控制信号）
- [ ] 迁移 `AgentFlowNode` → `LeafNode`
- [ ] 迁移 `TaskNode` → 考虑新增 `LeafTaskNode`（用户回调签名需改变）
- [ ] 迁移 `ConditionNode` → `LeafNode`（只读 state + goto 控制信号）
- [ ] 考虑将 `FlowNode` 标记为 `#[deprecated]`
- [ ] 拆分 `node_context.rs` (575 行) → `execution_engine.rs` + `node_context.rs`
- [ ] 拆分 `execution_loop.rs` (469 行) → 拆出 `barrier_wait.rs`
- [ ] 清理 `workflow_state.rs:122` 中提及 BranchState/Overlay/ChangeLog 的过时注释

---

## 十一、v0.4 总体状态总结（2026-06-29）

> 基于代码实际状态的全面审计。
> 详见 [Plan vs Reality 对照表](../discuss/v04-plan-vs-reality.md)。

### Phase 进度

| Phase | 描述 | 进度 | 关键阻塞 |
|-------|------|------|----------|
| Phase 1 — ExecutionEngine | 已有，字段与 Plan 一致 | ✅ 100% | 文件位置不佳（node_context.rs） |
| Phase 2 — 删除 BranchState | 已完全删除 | ✅ 100% | 无 |
| Phase 3 — Node API 统一 | LeafNode 已有，FlowNode 待迁移 | ⚠️ 60% | ConditionNode, BarrierNode, TaskNode |
| Phase 4 — Composite Node | ParallelNode 已在 ExecutorOperation | ✅ 100% | branches 仍用 FlowNode |
| Phase 5 — Streaming 统一 | run_inline_stream 不存在 | ❌ 0% | 需确认 API 需求 |
| Phase 6 — LlmInvoker | Retry + Fallback + Stream SM | ✅ 100% | 无 |
| Phase 7 — Checkpoint | 架构完成，未集成 | ⚠️ 50% | execution_loop 未接入 |
| Phase 8 — ExecutionTrace | 类型完成，未集成 | ⚠️ 30% | execution_loop 未接入 |

**总体进度：约 55-60%**

### 剩余工作量估算

| 任务 | 复杂度 | 预估 |
|------|--------|------|
| ConditionNode → LeafNode | 低 | 1h |
| BarrierNode → LeafNode | 低 | 1h |
| TaskNode 决策（保留/迁移） | 低 | 0.5h |
| Checkpoint 集成到 execution_loop | 中 | 4-8h |
| Checkpoint 恢复路径 | 高 | 8-16h |
| ExecutionTrace 集成 | 低 | 2h |
| 文件拆分（3 个） | 低 | 2h |
| 注释清理 | 低 | 0.5h |
| **合计** | | **~18-30h** |

### 文件健康问题

| 文件 | 行数 | 建议 |
|------|------|------|
| `node_context.rs` | 575 | 拆分为 execution_engine.rs + node_context.rs |
| `execution_loop.rs` | 469 | 拆出 barrier_wait.rs |
| `graph.rs` | 534 | 拆出 graph_builder.rs（可选） |
