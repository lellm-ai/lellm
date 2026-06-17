# LeLLM v0.3 架构演进

> 版本：v0.3 规划 | 日期：2026-06-16 | 状态：Phase 1~4 已实现 ✅
>
> **原则：** 本文档记录所有 v0.3 设计决策，与 v0.2 代码严格区分。
>
> **实现进度：**
> - ✅ 一、Crate 架构重构（lellm-runtime crate 已创建）
> - ✅ 二、StateDelta + Reducer 状态系统（DeltaOp, Reducer, ReducerRegistry 已实现）
> - ✅ 三、ParallelNode 状态合并策略（已实现 — NodeKind::Parallel, ParallelNode, handle_parallel, merge_deltas DeltaOp fallback）
> - ✅ 四、Checkpoint + Resume + ExecutionTrace（结构体已定义）
> - ✅ 五、错误模型重构（RecoverableError 已删除，Fallback 改为控制流）
> - ✅ 六、Executor 语义修复（handle_continue/handle_barrier/handle_fallback/handle_error 全部分裂完成）
> - ✅ 七、Builder 验证与分析分离（build() 纯函数化，GraphDiagnostics 已定义，Graph::analyze() 已实现）
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
├── ParallelNode（已实现 ✅）
├── FlowEvent, FlowNode trait
└── 编排层
    ↑ 依赖 runtime, core（需要 Message/ToolCall 等核心类型保证类型安全）
    ❌ 绝不依赖 agent
    ❌ 绝不依赖 provider
```

### 依赖图

```
graph → runtime + core
agent → runtime + core + provider
provider → core
```

**lellm-graph 是真正的通用工作流引擎。** 类似 LangGraph / Temporal / Prefect / Airflow。

### 使用场景

**纯工作流（无 LLM）：**
```toml
lellm-graph = "0.3"
# 拉入 core（Message/ToolCall 类型），但不拉入 provider, agent, openai, anthropic
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
    pub key: Cow<'static, str>,
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

### DeltaOp 与 Reducer 统一（已决策）

**核心洞察：Reducer 已经定义了 Delta 的语义，DeltaOp 是多余的间接层。**

LangGraph 没有这个问题：节点只提供值，Reducer 决定如何合并。LeLLM 当前的 DeltaOp 是过度设计。

**统一后的设计：**

```rust
// DeltaOp 被大幅简化，只保留控制类操作
pub enum StateDelta {
    Put {
        key: StateKeyId,
        value: Value,
    },
    Delete {
        key: StateKeyId,
    },
}

// Reducer 负责处理多个 Put 的合并
pub enum Reducer {
    Replace,       // 最后写入者胜
    Append,        // 数组追加
    MergeObject,   // 对象浅合并
    Sum,           // 数值求和
    Max,
    Min,
    Custom(Box<dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync>),
}

// StateKey 携带 Reducer
pub static MESSAGES: StateKey<Vec<Message>> = StateKey::append("messages");
pub static SCORE: StateKey<i32> = StateKey::sum("score");

// 节点使用
ctx.emit(MESSAGES, vec![msg]);
ctx.emit(SCORE, 10);
```

**为什么 Delete 必须保留：** `messages.clear()` 这种控制操作无法由 Reducer 表达。

**ParallelNode 合并流程：**

```
BranchA: Put(messages, [msg1])
BranchB: Put(messages, [msg2])
         ↓
Reducer::Append
         ↓
[msg1, msg2]
```

**原来的 Set/Append/Sum/Max/Min/MergeObject 全部删除** — 它们本质上是 Reducer 的职责。

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
2. **P1**: StateDelta — ✅ 已实现，节点签名已迁移至 `&State` + `Vec<StateDelta>`
3. **P1**: ParallelNode + ReducerRegistry — ✅ 已实现（`NodeKind::Parallel`，`ParallelNode`，`handle_parallel`）

**关键洞察：State Merge 是前置条件，ParallelNode 不是。**

### ⚠️ StateDelta 当前局限

- **`key: String` 每次分配**：`StateKey.name` 为 `&'static str`，但 `StateDelta` 存储为 `String`。
  已改为 `Cow<'static, str>`，对已知 key 零分配，同时支持动态 key 场景。

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

### CheckpointPolicy：图级执行策略

**核心洞察：Checkpoint 价值 = 重算成本 × 失败概率，而非 State 大小。**

Checkpoint 不是节点属性，而是图的执行策略。同一个 AgentNode 在不同图中价值完全不同：

```
场景 A: load → agent → save          重跑 3 秒，$0.001 → 不需要 Checkpoint
场景 B: crawl_100 → agent → review   重跑 15 分钟，100 万 token → 必须 Checkpoint
```

```rust
pub enum CheckpointTrigger {
    BarrierResolved,       // 默认开启 — ParallelNode 合并后天然恢复点
    ExecutionCompleted,    // 默认开启 — 最终结果 = 最后一个 Checkpoint
    HumanDecision,         // 强烈建议 — 审批后立即存，避免恢复时重复请求审批
    Explicit,              // 显式标注 — builder.node("agent", agent).checkpoint()
    Adaptive,              // v0.4 — 基于 ExecutionMetadata 动态决策
}

pub struct CheckpointPolicy {
    pub triggers: Vec<CheckpointTrigger>,
}

impl CheckpointPolicy {
    pub fn conservative() -> Self { /* Barrier + Completion + HumanDecision */ }
    pub fn minimal() -> Self { /* Barrier + Completion only */ }
}
```

**HumanDecision 触发器的必要性：**

```
agent → barrier(human approval) → deploy

审批通过后立即存。
否则恢复一次 → 审批一次 → 恢复一次 → 审批一次，产生副作用。
```

**Adaptive 方向（v0.4）：**

```rust
struct ExecutionMetadata {
    duration_ms: u64,
    token_cost: f64,
    external_side_effects: bool,
}

// CheckpointScore = duration + token_cost + side_effect_weight
// TaskNode (2ms) → 不存，AgentNode (90s) → 存，DeployNode → 一定存
```

**StateDelta 降低 Checkpoint 成本：**

```rust
struct StateSnapshot {
    base_snapshot: State,       // 上次 checkpoint 的完整 State
    recent_deltas: Vec<Delta>, // 两次 checkpoint 间的增量
}
// 恢复时：base + apply(deltas) → 避免频繁全量序列化
```

### ⚠️ Checkpoint 缺失的设计

**Storage SPI 未定义：** `Checkpoint` 结构体已定义，但无 `CheckpointStore` trait：
- 存到哪里？（SQLite / S3 / 内存）
- 如何查询？（按 trace_id？按 graph_hash？）
- 如何清理过期 Checkpoint？

**v0.4 优先级：** 定义 `CheckpointStore` trait + 内存后端实现。

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

- **Builder 层：** end 节点有出边 → `GraphDiagnostics` Warning（`Graph::analyze()` 已实现 ✅）
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

**`build()` = 结构正确性校验（纯函数），保证图能跑**
**`analyze()` = 风险诊断（可观测性），不阻止执行**

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

**✅ 实现状态：** `GraphDiagnostics` 结构体已定义，`Graph::analyze()` 方法已实现。
覆盖以下诊断维度：环检测、Fallback 参与循环、不可达路径、End 节点出边。
`analyze_cycles()` 保留为兼容方法，标记为 @deprecated。

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
    fn before_tool(&self, ctx: &ToolCallContext, state: &State) -> Vec<StateDelta>;
    fn after_tool(&self, ctx: &ToolCallContext, result: &ToolResult, state: &State) -> Vec<StateDelta>;
    fn after_iteration(&self, snapshot: &IterationSnapshot, state: &State) -> Vec<StateDelta>;
}
```

**Hook 输出 Deltas，AgentFlowNode 收集后统一 apply：**

```rust
let hook_deltas: Vec<StateDelta> = hooks.iter()
    .flat_map(|h| h.before_tool(&ctx, &state))
    .collect();
state.apply_deltas(hook_deltas);  // 经过 Reducer，进 ExecutionTrace
```

### 设计决策矩阵

| 问题 | 选择 | 理由 |
|------|------|------|
| 同步还是异步 | **同步** | 轻量扩展点（O(μs~ms)） |
| 能否修改 State | **通过 Delta** | 与节点 Delta 哲学一致，所有修改经过 Reducer + ExecutionTrace |
| 能否修改 ToolCall | **不允许** | `&ToolCall` — 禁止中间件行为 |
| 能否修改 ToolResult | **不允许** | 保持执行链路可追溯 |
| Hook 失败 | **ObservedError** | Hook = 附加能力 |
| 是否中断 Agent | **否** | Hook 失败不影响 Agent 循环 |
| 是否允许异步 I/O | **否** | 通过 State → 下游 Node |
| 是否承担审批逻辑 | **否** | BarrierNode 负责 |

### 为什么禁止修改 ToolCall

```
LLM 认为调用 A → 实际执行 A'
```

ExecutionTrace 记录与实际操作不一致，调试极其痛苦。正确做法：脱敏/校验在 Tool 层完成。

---

## 九、Event 体系解耦

### 改前（耦合）

```rust
enum NodeEvent {
    Agent(lellm_agent::AgentEvent),  // ← graph 依赖 agent
}
```

### 改后（解耦）— lellm-events 作为一等公民

**事件协议提升为独立 crate：**

```
lellm-core       — 数据模型（Message, ToolCall, ToolResult, Usage, ModelResponse, StateValue）
lellm-events     — 事件协议（AgentEvent, GraphEvent, NodeEvent, FlowEvent, TraceId, SpanId）
lellm-runtime    — 执行基础设施（EventSink<Event>）
lellm-graph      — DAG / Flow Engine（emit GraphEvent）
lellm-agent      — ReAct Runtime（emit AgentEvent）
lellm-provider   — Model Adapter
```

### 干掉 Extension 逃生舱口

**`Extension { kind: String, payload: Value }` 是把 enum 重新发明成 String，丢掉编译器检查。**

95% 的 Extension 永远不会被规范化。真正需要的是 `Unknown` 变体：

```rust
pub enum AgentEvent {
    IterationStarted { ... },
    IterationCompleted { ... },
    ToolCallStarted { ... },
    ToolCallCompleted { ... },
    Finished { ... },
    Unknown {                        // 90% 事件有类型安全，只有新增事件走逃生舱
        name: String,
        payload: Value,
    },
}
```

### Graph 永远不知道 Agent 的存在

```rust
pub enum NodeEvent {
    Started { node_id, span_id },
    Completed { node_id, span_id, duration },
    Failed { node_id, error },
    Custom(Box<dyn Any>),            // 类型安全的向下转换
}
```

Agent Adapter 负责映射：`AgentEvent` → `NodeEvent`。Graph 只处理 `NodeEvent`。

### AgentEvent 定位

**不是协议数据，而是执行时遥测（telemetry）数据。** 放在 `lellm-events` 而非 `lellm-core`。

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

### ✅ 第五步：引入 StateDelta（已完成）

节点签名已从 `fn(&mut State) -> NextStep` 改为 `fn(&State) -> NodeOutput { deltas, next }`。
`FlowNode` trait 签名已变更，所有节点和测试已完成迁移。

### ✅ 第六步：ParallelNode（已完成）

`NodeKind::Parallel` 已实现，支持分支 fork/merge + ReducerRegistry 冲突解决。

### ✅ 第七步：完善 GraphDiagnostics（已完成）

`Graph::analyze()` 方法已实现，返回 `GraphDiagnostics`。`analyze_cycles()` 保留为兼容方法。

### ⏳ 第八步：删除废弃类型（待评估）

第五、六、七步已完成。`EdgeAnalysis`、`CycleAnalysis`、`PendingEdge::max_visits()` 暂时保留以支持 `analyze_cycles()` 兼容方法。v0.4 评估是否移除。
