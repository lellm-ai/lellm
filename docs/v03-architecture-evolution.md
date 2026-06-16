# LeLLM v0.3 架构演进

> 版本：v0.3 规划 | 日期：2026-06-16 | 状态：Phase 1~4 已实现 ✅
>
> **原则：** 本文档记录所有 v0.3 设计决策，与 v0.2 代码严格区分。
>
> **实现进度：**
> - ✅ 一、Crate 架构重构（lellm-runtime crate 已创建）
> - ✅ 二、StateDelta + Reducer 状态系统（DeltaOp, Reducer, ReducerRegistry 已实现）
> - ⏳ 三、ParallelNode 状态合并策略（P2，未开始 — NodeKind 无 Parallel 变体）
> - ✅ 四、Checkpoint + Resume + ExecutionTrace（结构体已定义）
> - ✅ 五、错误模型重构（RecoverableError 已删除，Fallback 改为控制流）
> - ✅ 六、Executor 语义修复（handle_continue/handle_barrier/handle_fallback/handle_error 全部分裂完成）
> - ✅ 七、Builder 验证与分析分离（build() 纯函数化，GraphDiagnostics 已定义）
> - ✅ 八、AgentHook 可观测性扩展点（已实现，v0.3 短期妥协 &mut State）
> - ✅ 九、Event 体系解耦（FlowEvent 替代 NodeEvent::Agent）
> - ✅ 十、Executor 重构（Done+Observed 合并为 Continue，handle_continue/handle_barrier/handle_fallback/handle_error 全部分裂）
> - ✅ 十一、删除清单（RecoverableError, BuildError::Warning 已删除）

---

## 目录

- [一、Crate 架构重构](#一crate-架构重构)
- [二、StateDelta + Reducer 状态系统](#二statedelta--reducer-状态系统)
- [三、ParallelNode 状态合并策略](#三parallelnode-状态合并策略)
- [四、Checkpoint + Resume + ExecutionTrace](#四checkpoint--resume--executiontrace)
- [五、错误模型重构](#五错误模型重构)
- [六、Executor 语义修复](#六executor-语义修复)
- [七、Builder 验证与分析分离](#七builder-验证与分析分离)
- [八、AgentHook 可观测性扩展点](#八agenthook-可观测性扩展点)
- [九、Event 体系解耦](#九event-体系解耦)
- [十、Executor 重构](#十executor-重构)
- [十一、删除清单](#十一删除清单)
- [十二、迁移路径](#十二迁移路径)

---

## 一、Crate 架构重构

### 核心问题

当前依赖方向反直觉——最通用的 Graph 层硬依赖最特定的 Agent 层：

```
lellm-graph（最通用）
  ↓ 硬依赖
lellm-agent（特定领域）
  ↓
lellm-provider
  ↓
lellm-core
```

纯 ETL 用户（只用 TaskNode）被迫拉入 provider + agent + openai sdk + anthropic sdk。

### 目标架构

```
lellm-core
├── Message, ToolCall, ModelRequest, ModelResponse
└── 通用协议

lellm-provider
└── OpenAI / Anthropic / Ollama ...
    ↑ 依赖 core

lellm-agent
├── Agent, ReAct, ToolLoop, ContextCompaction
├── RetryPolicy, AgentHook
└── AgentFlowNode（实现 FlowNode trait）
    ↑ 依赖 core, provider

lellm-runtime
├── State, StateDelta
├── Reducer
├── Checkpoint
└── ExecutionTrace
    └── 无外部依赖 — 全系统基础设施

lellm-graph
├── Graph, GraphBuilder, GraphExecutor
├── TaskNode, ConditionNode, BarrierNode
├── ParallelNode（P2，未开始）
├── FlowEvent, FlowNode trait
└── 编排层
    ↑ 依赖 runtime
    ❌ 绝不依赖 agent
    ❌ 绝不依赖 provider
    ❌ 绝不依赖 core
```

### 依赖图

```
graph → runtime
agent → runtime + core + provider
provider → core
```

**lellm-graph 是真正的通用工作流引擎。** 类似 LangGraph / Temporal / Prefect / Airflow。

### 使用场景

**纯工作流（无 LLM）：**
```toml
lellm-graph = "0.3"
# 不拉入 provider, agent, openai, anthropic
```

**Agent 工作流：**
```toml
lellm-graph = "0.3"
lellm-agent = "0.3"
lellm-provider = { version = "0.3", features = ["anthropic"] }
```

### Node 体系：trait 而非 enum 爆炸

```rust
pub trait FlowNode: Send + Sync {
    async fn execute(
        &self,
        ctx: &mut FlowContext,
    ) -> Result<NodeResult, GraphError>;
}
```

Graph 不知道 `AgentNode` 是什么，只知道 `dyn FlowNode`。

`AgentNode` 搬出 graph crate，由 agent crate 提供 `AgentFlowNode`：

```rust
use lellm_agent::AgentFlowNode;
graph.node("planner", AgentFlowNode::new(agent));
```

---

## 二、StateDelta + Reducer 状态系统

### 核心设计

**键级 StateDelta + ReducerRegistry。** 不上路径级，不上 RFC 6902。
LeLLM 做的是 Graph Runtime 的状态系统，不是通用 JSON Patch 工具。

### StateDelta 结构

```rust
pub struct StateDelta {
    pub key: String,
    pub op: DeltaOp,
    pub value: Value,
}
```

### DeltaOp 枚举

```rust
pub enum DeltaOp {
    Set,           // 覆盖
    Delete,        // 删除 key
    Append,        // 追加到数组（目标必须是 Array）
    MergeObject,   // 浅合并 object（目标必须是 Object）
    Sum,           // 数值相加（目标必须是 Number）
    Max,           // 取较大值（目标必须是 Number）
    Min,           // 取较小值（目标必须是 Number）
}
```

**Apply 时类型不匹配 → `StateError`。**

### Reducer 枚举

```rust
pub enum Reducer {
    Error,         // 默认 — 冲突即报错
    Replace,       // 最后写入者胜
    Append,        // 数组追加
    MergeObject,   // 对象合并
    Sum,           // 数值求和
    Max,
    Min,
    Custom(Box<dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync>),
}
```

### Reducer 与 DeltaOp 的关系

**独立设计：**
- `DeltaOp` 描述"我想做什么"（修改意图）
- `Reducer` 描述"这个 key 允许怎么合并"（合并策略）

Apply 时检查 DeltaOp 是否被该 key 的 Reducer 允许。

### ⚠️ Reducer 与 DeltaOp 的变体重叠

当前 DeltaOp 和 Reducer 的变体高度重合（6/7 一一对应）：

| DeltaOp | Reducer | 重叠？ |
|---------|---------|--------|
| Set | Replace | 是 |
| Append | Append | 是 |
| MergeObject | MergeObject | 是 |
| Sum | Sum | 是 |
| Max | Max | 是 |
| Min | Min | 是 |
| Delete | （无） | 独有 |
| （无） | Error | 独有 |

**为什么保留两套？** DeltaOp 是节点发出的修改意图（运行时必需），Reducer 是 key 注册的合并策略（并行冲突解决）。
两者语义不同，但当前单分支执行路径中，Reducer 仅在 `merge_deltas()` 的多 writer 场景生效。

**v0.4 候选优化：** 统一为单一枚举，`DeltaOp` 同时承担意图与策略。前提是确认没有 `DeltaOp::Append` + `Reducer::Replace` 这样的交叉场景。

### StateKey<T> 绑定 Reducer

Key、Type、Reducer 三者绑定：

```rust
pub static MESSAGES: StateKey<Vec<Message>> = 
    StateKey::new("messages", Reducer::Append);
```

### 节点签名变更

```rust
// 改前 — 直接修改共享 State
fn execute(&mut State) -> NextStep

// 改后 — 输出 Delta，不修改 State
fn execute(&State) -> NodeOutput

struct NodeOutput {
    deltas: Vec<StateDelta>,
    next: NextStep,
}
```

---

## 三、ParallelNode 状态合并策略

### 核心原则

**节点产生 StateDelta（patch），不直接修改共享 State。冲突即报错。**

### 为什么不是直接合并最终 State

两个分支各写 `"count": 2`，merge 时系统无法区分"两次 +1"还是"恰好写了相同值"——信息已丢失。

```
Fork → Branch A (delta: count += 1) → Merge
      → Branch B (delta: count += 1) → Apply Delta → count = 3
```

### 执行模型

```
State
 ↓
fork
 ↓
A     B
 ↓     ↓
StateDelta  StateDelta
 ↓     ↓
ReducerRegistry Merge
 ↓
New State
```

### 默认行为

未注册 Reducer 的 key 发生并行写入 → `StateConflict`：

```rust
StateConflict {
    key: "result",
    writers: ["agentA", "agentB"],
}
```

### v0.3 优先级

1. **P0**: StateKey<T> — ✅ 已实现（`lellm-runtime/src/statekey.rs`）
2. **P1**: StateDelta — ✅ 结构体已实现，但节点签名尚未迁移（仍为 `&mut State`）
3. **P2**: ParallelNode + ReducerRegistry — ⏳ 未开始（`NodeKind` 无 `Parallel` 变体）

**关键洞察：State Merge 是前置条件，ParallelNode 不是。**

### ⚠️ StateDelta 当前局限

- **`key: String` 每次分配**：`StateKey.name` 为 `&'static str`，但 `StateDelta` 存储为 `String`。
  v0.4 应改为 `Cow<'static, str>`，对已知 key 零分配。
- **节点签名未迁移**：`FlowNode::execute()` 仍为 `fn(&mut State) -> NextStep`，未输出 `Vec<StateDelta>`。
  StateDelta 目前仅在 `ReducerRegistry::apply_delta()` / `merge_deltas()` 中可用，未被节点生产。

---

## 四、Checkpoint + Resume + ExecutionTrace

### 核心区分

**Checkpoint 负责恢复，ExecutionTrace 负责审计。** 两者是完全独立的对象。

### Checkpoint — Materialized State + Execution Cursor

```rust
struct Checkpoint {
    checkpoint_id: CheckpointId,
    parent_trace_id: TraceId,      // 关联原始执行
    graph_hash: String,            // 图结构快照
    current_node: NodeId,          // 执行游标 — 从哪继续
    state: State,                  // 完整物化快照（所有 Delta 已 apply）
    created_at: DateTime,
}
```

| 决策 | 选择 | 理由 |
|------|------|------|
| 存储内容 | **完整物化 State** | 恢复时无需 replay，直接 load + continue |
| Delta 的角色 | **不进入 Checkpoint** | Delta 只存在于 ExecutionTrace，用于审计 |
| 图变更校验 | **两级：Strict / Force** | 默认拒绝，Force 显式承担风险 |

### ExecutionTrace — Delta 历史

```rust
struct ExecutionTrace {
    trace_id: TraceId,
    initial_state: State,
    entries: Vec<ExecutionEntry>,
    deltas: Vec<StateDelta>,        // 每个节点的修改意图
    barrier_decisions: Vec<BarrierDecision>,
}
```

**Delta 的用途：** 可视化、调试、审计。**Delta 绝不用于 Checkpoint 恢复。**

### Resume 语义

```
load(checkpoint)
  ↓
校验 graph_hash
  ↓
校验 current_node 存在
  ↓
load(state)
  ↓
continue from current_node
```

### 图变更校验：两级模式

```rust
enum GraphHashMode {
    Strict,  // 默认 — hash 不同则拒绝
    Force,   // 高级 — hash 不同则 warn + 继续
}
```

### Checkpoint 与 Reducer 的解耦

Checkpoint 永远只保存 Materialized State：
- 不知道 ReducerRegistry
- 不知道 Append/Replace/Custom
- 不依赖 Delta 序列

### ⚠️ Checkpoint 缺失的设计

**Storage SPI 未定义：** `Checkpoint` 结构体已定义，但无 `CheckpointStore` trait：
- 存到哪里？（SQLite / S3 / 内存）
- 如何查询？（按 trace_id？按 graph_hash？）
- 如何清理过期 Checkpoint？

**频率策略未确定：** 每次节点执行都存？还是仅 Barrier 后存？
长对话场景（100 条消息 × 5KB = 500KB/Checkpoint），频繁序列化成本高。

**v0.4 优先级：** 定义 `CheckpointStore` trait + 内存后端实现，明确频率策略。

### 绝不采用的方案

| 方案 | 原因 |
|------|------|
| 纯 Delta Chain Checkpoint | 恢复时需要 replay，图变了就失效 |
| 默认允许图变更后 Resume | 用户可能不知道图已变，静默行为危险 |
| 恢复时重放历史 Delta | LLM/Tool/Barrier 不可重放，物化快照更可靠 |

---

## 五、错误模型重构

### 核心洞察：三种完全不同的概念

| 概念 | 含义 | 层级 | 现有归属 |
|------|------|------|----------|
| **Retry** | 重试当前操作（网络超时、429） | 节点内部 | `RetryPolicy` — 正确 |
| **Fallback** | 主路径失败，切换备用路径 | 图级控制流 | `RecoverableError` — **错误** |
| **Ignore** | 非关键失败，继续执行 | 可观测性 | `ObservedError` — 正确 |

**问题：`RecoverableError` 同时承担了 Retry 和 Fallback 两种职责，且几乎无生产者（死代码）。**

### 决策：删除 RecoverableError

```rust
// 改前
enum GraphError {
    Terminal(TerminalError),
    Recoverable(RecoverableError),  // ← 删除
}

// 改后
enum GraphError {
    Terminal(TerminalError),    // 终止执行
}
```

### ✅ Fallback 变成控制流

```rust
enum StreamNodeResult {
    Continue { next, span_id, observed },  // 统一 Done + Observed
    Pause { barrier_id, .. },              // Barrier 专用
    Fallback { reason, node_name },        // 节点主动声明走备用路径
}
```

**关键区别：** `RecoverableError` 是"错误"，语义模糊；`StreamNodeResult::Fallback` 是"控制流"，节点主动声明。

**完整调用链：**
```
节点返回 StreamNodeResult::Fallback { reason }
  → Executor.handle_fallback()
    → graph.find_fallback_edge(current)
      → 有边：发送 ObservedError::Degraded + 路由到备用节点
      → 无边：发送 GraphError::Terminal + 终止
```

### Fallback 边验证

**`edge_fallback("A", "A")` — Build Error**

这是 retry，不是 fallback。职责边界必须清晰。

**Fallback 参与循环 — Cycle Analysis Warning**

Builder 无法静态证明，由 `analyze()` 输出 Warning。

---

## 六、Executor 语义修复

### ✅ Stream Consumer Drop = Cancel Execution

`send(event)` 失败时立即 `return`，executor 终止：

```rust
// send() 返回 true 表示 consumer 已断开
async fn send(&self, event_tx: &mpsc::Sender<GraphEvent>, event: GraphEvent) -> bool {
    event_tx.send(event).await.is_err()
}

// 主循环中：
if self.send(&event_tx, GraphEvent::GraphStart { trace_id }).await {
    return;  // consumer dropped, graceful shutdown
}
```

**不发送 GraphComplete，也不发送 GraphError。** 因为已经没有接收者。

### ✅ End Node 语义

End Node 执行完成 → 立即终止 → 忽略 NextStep。

- **Builder 层：** end 节点有出边 → `GraphDiagnostics` Warning（待实现 `Graph::analyze()`）
- **Runtime 层：** `handle_continue()` / `handle_barrier()` 中，`current == graph.end_node()` → `StepOutcome::Break`

end 节点会被正常执行（如 summary/cleanup），执行后无条件终止。

### ✅ Fallback 控制流完整链路

```
节点返回 StreamNodeResult::Fallback { reason }
  ↓
Executor 调用 handle_fallback()
  ↓
查找 graph.find_fallback_edge(current)
  ↓
有 fallback 边 → 发送 ObservedError::Degraded 事件 → 路由到备用节点
无 fallback 边 → 发送 GraphError::Terminal → 终止执行
```

---

## 七、Builder 验证与分析分离

### 核心原则

**`build()` = 结构正确性校验（纯函数）**
**`analyze()` = 风险诊断（可观测性）**

### build() 的职责

只检查结构性问题：节点存在、边引用有效、入口/出口存在、Fallback 不指向自身。

```rust
let graph = builder.build()?;  // Result<Graph, BuildErrors>
```

**绝不产生 Warning。** 不打 `tracing::warn!`。纯函数。

### analyze() 的职责

检查风险性问题：环检测、Fallback 参与循环、不可达路径、条件边重叠、End 节点有出边。

```rust
let diagnostics = graph.analyze();
for w in diagnostics.warnings() {
    println!("warning: {}", w);
}
```

**⚠️ 实现状态：** `GraphDiagnostics` 结构体已定义，但 `Graph::analyze()` 方法未实现。
当前仅有 `Graph::analyze_cycles()` 返回 `CycleAnalysis`（基于 `EdgeAnalysis` 的 `max_visits` 约束）。
`analyze()` 的完整实现是 P2 优先级任务。

### 类型设计

```rust
pub enum BuildError {
    DuplicateNode { id: String },
    MissingNode { from: String, to: String },
    MissingEntryPoint,
    MissingExitPoint,
    InvalidFallback { node: String, reason: String },
}
// 删除 BuildError::Warning 变体

pub struct GraphDiagnostics {
    pub warnings: Vec<Diagnostic>,
    pub infos: Vec<Diagnostic>,
}

pub struct Diagnostic {
    pub severity: Severity,     // Info | Warning
    pub category: DiagnosticCategory,
    pub message: String,
}
```

---

## 八、AgentHook 可观测性扩展点

### 核心定位

**AgentHook = Graph Runtime Extension，不是 Agent Middleware。**

```
观测 · 记录 · 标记 · 注入元数据

不是
路由 · 审批 · 重写 · 策略执行
```

### API 设计

```rust
pub trait AgentHook: Send + Sync {
    fn before_tool(&self, ctx: &ToolCallContext, state: &mut State);
    fn after_tool(&self, ctx: &ToolCallContext, result: &ToolResult, state: &mut State);
    fn after_iteration(&self, snapshot: &IterationSnapshot, state: &mut State);
}
```

### 设计决策矩阵

| 问题 | 选择 | 理由 |
|------|------|------|
| 同步还是异步 | **同步** | 轻量扩展点（O(μs~ms)） |
| 能否修改 State | **可以（v0.3 妥协）** | 短期实用，已知与 Delta 哲学矛盾（见下方⚠️） |
| 能否修改 ToolCall | **不允许** | `&ToolCall` — 禁止中间件行为 |
| 能否修改 ToolResult | **不允许** | 保持执行链路可追溯 |
| Hook 失败 | **ObservedError** | Hook = 附加能力 |
| 是否中断 Agent | **否** | Hook 失败不影响 Agent 循环 |
| 是否允许异步 I/O | **否** | 通过 State → 下游 Node |
| 是否承担审批逻辑 | **否** | BarrierNode 负责 |

### ⚠️ `&mut State` 妥协 — 已知问题

**v0.3 允许 Hook 直接 `&mut State`，与第 2 节"节点输出 Delta"原则矛盾。**

后果：
1. **审计盲区**：Hook 修改的 State 不会进入 `ExecutionTrace` 的 Delta 历史
2. **并行未定义**：ParallelNode 场景下，Hook 的执行顺序不确定
3. **v0.4 必须收敛**：`before_tool(&ToolCall, &State) -> Vec<StateDelta>`

**为什么 v0.3 不直接改为 Delta？** AgentHook 当前仅在 AgentFlowNode 内部同步调用，
不涉及并行执行。引入 Delta 输出会增加一层 `Vec<StateDelta>` 收集逻辑，
而 AgentFlowNode 的主循环尚未完全迁移到 Delta 模式。**代价 > 收益，推迟到 v0.4。**

### 为什么禁止修改 ToolCall

```
LLM 认为调用 A → 实际执行 A'
```

ExecutionTrace 记录与实际操作不一致，调试极其痛苦。正确做法：脱敏/校验在 Tool 层完成。

### 演进路线

- **v0.3**: `before_tool(&ToolCall, &mut State)` — 简单落地（已知妥协）
- **v0.4+**: `before_tool(&ToolCall, &State) -> HookOutput { deltas: Vec<StateDelta>, observed: Option<ObservedError> }` — 与 Delta 体系统一

---

## 九、Event 体系解耦

### 改前（耦合）

```rust
enum NodeEvent {
    Agent(lellm_agent::AgentEvent),  // ← graph 依赖 agent
}
```

### 改后（解耦）

```rust
pub enum FlowEvent {
    NodeStarted { node_id: String, span_id: SpanId },
    NodeCompleted { node_id: String, span_id: SpanId, duration: Duration },
    NodeFailed { node_id: String, error: GraphError },
    BarrierWaiting { barrier_id: BarrierId, node_id: String },
    BarrierResolved { barrier_id: BarrierId, decision: BarrierDecision },
    StateChanged { node_id: String, delta: StateDelta },
    ExecutionCompleted { result: GraphResult },
    ExecutionFailed { error: GraphError },
    Extension { node_id: String, payload: serde_json::Value },
}
```

**Graph 不知道 `AgentEvent`、`ToolCall`、`ToolResult`。**

Agent 通过 `Extension` 变体注入内部事件。

### ⚠️ `Extension { payload: serde_json::Value }` 逃生舱口问题

`serde_json::Value` 是万能逃逸口——任何节点都可以往里塞任意 JSON。这破坏了 Event 体系的结构化优势：
下游消费者无法类型安全地 match 事件，只能 `payload.get("...").as_str()`。

**当前取舍：** v0.3 保留 `Value`，因为 Agent 内部事件体系尚未稳定。
**v0.4 候选方案：** 定义 `ExtensionKind` 枚举（有限扩展），或引入 `dyn Any` 的类型安全向下转换。

---

## 十、Executor 重构

### 问题

当前 executor 主循环 ~250 行，4 个 match 分支（Done/Observed/Barrier/Error），分支内逻辑重复。

### ✅ 重构已完成

主循环（`run_loop`）通过 `match result` 分派到 4 个独立处理方法：

```rust
match result {
    Ok(StreamNodeResult::Continue { .. }) => handle_continue(...).await,
    Ok(StreamNodeResult::Pause { .. })    => handle_barrier(...).await,
    Ok(StreamNodeResult::Fallback { .. }) => handle_fallback(...).await,
    Err(e)                                 => handle_error(...).await,
}
```

- `handle_continue()` — 执行日志 + NodeEnd 事件 + end 节点检查 + resolve_next
- `handle_barrier()` — BarrierWaiting 事件 + 等待决策 + BarrierResolved 事件 + 应用决策 + 路由解析
- `handle_fallback()` — 查找 fallback 边 + 发送 Degraded ObservedError + 路由到备用节点（无 fallback 边则终止）
- `handle_error()` — 执行日志 + NodeEnd(failure) 事件 + GraphError 事件

**`handle_fallback()` 是文档最初未预见的第五个方法，对应 `StreamNodeResult::Fallback` 控制流。**

### Done + Observed 合并

两者 90% 逻辑重复，统一为 `Continue(NodeOutput)`：

```rust
enum NodeOutput {
    Success,
    SuccessWithObservation(ObservedError),
}
```

### 长期方向：State Machine Executor

v0.3 引入 StateDelta 后，Executor 演化为：

```
execute node
  ↓
apply delta
  ↓
resolve outcome
  ↓
transition
  ↓
if transitioned_to_end {
    mark_final_step
}
```

---

## 十一、删除清单

| 类型 | 状态 | 理由 |
|------|------|------|
| `LoopNode` | ✅ 已删除 | 有环图已覆盖，无存在价值 |
| `EdgePolicy` | ✅ 已删除 | MaxVisits 撑不起抽象层，YAGNI |
| `RecoverableError` | ✅ 已删除 | 职责混淆，Fallback 改为控制流 |
| `BuildError::Warning` | ✅ 已删除 | Warning 不是 Error，迁移至 `GraphDiagnostics` |
| `EdgeAnalysis` | ⏳ 保留 | `analyze_cycles()` 仍在活跃使用，迁移至 `GraphDiagnostics` 未完成 |
| `TraceId` | ⏳ 保留 | Executor 已在生成 TraceId，暂不移除 |

---

## 十二、迁移路径

### ✅ 第一步：创建 lellm-runtime crate

State / StateDelta / Reducer / StateKey / Checkpoint / ExecutionTrace 已在 `lellm-runtime`。

### ✅ 第二步：graph 依赖 runtime，移除 agent 依赖

`lellm-graph` 仅依赖 `lellm-runtime`。`AgentFlowNode` 由 `lellm-agent` 提供。

### ✅ 第三步：Event 体系解耦

`FlowEvent` 替代 `NodeEvent::Agent`。Agent 通过 `Extension` 变体注入事件。

### ✅ 第四步：Executor 重构

`Done + Observed` 合并为 `Continue`。主循环拆为 `handle_continue` / `handle_barrier` / `handle_fallback` / `handle_error`。

### ⏳ 第五步：引入 StateDelta（P2）

节点签名从 `fn(&mut State) -> NextStep` 改为 `fn(&State) -> NodeOutput { deltas, next }`。
前置条件：`FlowNode` trait 签名变更（breaking change）。

### ⏳ 第六步：ParallelNode（P2）

在 `NodeKind` 中添加 `Parallel` 变体，实现分支 fork/merge + ReducerRegistry 冲突解决。

### ⏳ 第七步：完善 GraphDiagnostics（P2）

实现 `Graph::analyze()` 方法，替代 `analyze_cycles()`，返回 `GraphDiagnostics`。
届时可评估是否移除 `EdgeAnalysis` / `PendingEdge::max_visits()`。

### ⏳ 第八步：删除废弃类型（P2）

待第五、六、七步完成后，评估删除 `EdgeAnalysis`、`CycleAnalysis`、`PendingEdge::max_visits()`。
