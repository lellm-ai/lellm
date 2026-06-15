# v0.2 Graph — Grill 决策记录

> 日期：2026-06-15
> 分支：feature/v02-graph
> 状态：已收敛，代码已落地 ✅

---

## 1. Agent 三层能力模型

| 层级 | 能力 | 用户 | 版本 |
|------|------|------|------|
| Level 1（默认） | `AgentNode` — 黑盒 ReAct | 90% 用户 | v0.2 |
| Level 2（逃生口） | `LLMNode` + `ToolNode` — 手动搭建 ReAct | 高级用户 | v0.2 |
| Level 3（干预） | `AgentHook` — before_tool / after_tool / after_iteration | 需要轻量干预的用户 | v0.3+ |

**核心原则：**
- Graph 不负责表达 Agent 内部 ReAct
- `AgentNode` 是 Graph 中的原子执行单元
- 需要细粒度控制 → `LLMNode` + `ToolNode`
- 需要轻量干预 → `AgentHook`（v0.3+）

---

## 2. State 设计

### 2A 类型安全

```rust
type State = HashMap<String, serde_json::Value>;
```

- v0.2 保持动态类型，不引入 Schema
- 加强 `StateExt`：`get_str()`, `get_u64()`, `get_json::<T>()`, `require::<T>()`
- v0.3+ 考虑 `#[derive(GraphState)]` macro + `TypedState<T>`

### 2B Reducer 机制

**核心原则：Reducer 属于 State Model，不属于 Node Behavior。**

```rust
// Reducer 在 Builder 侧注册
graph.register_reducer("messages", Reducer::Append);
```

- 节点只声明"我产出了什么"
- State 决定"我怎么消化它"
- 不要把 Merge Policy 塞进 `GraphResult`

### 2C 并行分支预留

- v0.3 `ParallelNode` 提前预留 `StatePatch` + `ReducerRegistry` 概念
- 分支隔离：执行前 snapshot，执行后 delta merge

---

## 3. Edge 三层语义模型

```rust
struct Edge {
    to: NodeId,

    // ① 业务路由条件（必须满足）
    condition: Option<Expr>,

    // ② 分析用约束（不参与 runtime 决策）
    analysis: Option<EdgeAnalysis>,

    // ③ runtime policy（显式声明才生效）
    policy: Option<EdgePolicy>,

    // ④ fallback 标记
    fallback: bool,
}
```

**关键分界：**
- `analysis` = "你可能会出事"（静态分析用）
- `policy` = "我现在要拦你"（运行时拦截）

`max_visits` 归属 `EdgeAnalysis`（仅静态分析），不再默认 runtime 拦截。

显式 runtime 拦截用 `EdgePolicy::MaxVisits(u32)`。

### Edge Policy 被 Block 时的三种策略

| 策略 | 行为 | 适用场景 |
|------|------|----------|
| `STRICT`（默认） | 路径失败 → 回溯到上一个 decision node | Workflow, pipeline |
| `SOFT_FALLBACK` | 尝试其他满足 condition 的 edge → fallback edge → 失败 | Agent graph, tool routing |
| `DROP` | 跳过该 transition，不报错 | Best-effort |

### Fallback Edge

```rust
builder
    .edge("A", "B", condition: "score >= 80")
    .edge("A", "C", condition: "score < 50")
    .edge("A", "D", fallback: true); // 兜底
```

Runtime 规则：匹配 edge → 尝试 fallback edge → 仍无匹配 → `TerminalError::Unrouted`。

---

## 4. 循环保护

### 4A 三层系统

| 层级 | 机制 | 默认值 |
|------|------|--------|
| 全局 | `GraphExecutor::max_steps` | 50 |
| 边级 | `EdgePolicy::MaxVisits`（显式声明） | 无 |
| 语法糖 | `LoopNode::max_iterations` | - |

### 4B Step 定义

**1 Step = 1 Node Entry**

- 进入 Node 即 +1 step
- Node 内部执行（ReAct / tool / loop）不计 step
- Edge traversal 不单独计 step

### 4C 循环分析

`analyze_cycles()` 输出结构化报告：

```json
{
  "cycles": [
    {
      "path": ["A", "B", "C", "A"],
      "edges": [
        {"edge": "C->A", "analysis": { "max_visits": 2 }}
      ],
      "risk": "HIGH"
    }
  ]
}
```

- DFS 回溯 + 回溯路径报告
- 标注危险来源（cycle 本身 / max_visits 缺失 / policy 冲突）
- 不做强约束判断：只回答"这里可能炸，不负责修"

---

## 5. Barrier 设计

### 5A 定位

保持 `BarrierNode`（Graph 拓扑中的节点），不放边上。

### 5B 循环中多次到达

- 默认 **Per-Instance**：每次到达生成新 `BarrierId`，必须重新决策
- 可选 `memoize_decision()`：记住第一次决策，后续自动通过

### 5C 超时

- v0.2：超时 = Graph 结束，State 保留在返回结果中
- 用户负责决定是否需要重建 Graph 重试

### 5D BarrierId

```rust
struct BarrierId {
    node_id: String,   // 用户定义的节点名，可预测
    occurrence: u32,   // 第几次到达（1-based）
}
```

DecisionRegistry 支持通配决策：

```rust
handle.decide_wildcard("approve_deploy", BarrierDecision::Approve);
// 匹配所有 occurrence
```

---

## 6. 流式执行契约

### 6A API

```rust
fn execute_stream(graph, state) -> GraphExecution {
    GraphExecution {
        stream: GraphStream,      // 观察权（read-only view）
        handle: GraphHandle,      // 控制权（write + cancel）
        cancel_token: CancellationToken,
    }
}
```

**核心原则：Stream is primary, Blocking is derived.**

```rust
// 阻塞模式 = 消费 stream 直到结束
fn execute(&graph, state) -> Result<GraphResult, GraphError> {
    let exec = execute_stream(graph, state);
    consume_stream(exec.stream)
}
```

### 6B 生命周期规则

| 操作 | 效果 |
|------|------|
| `handle.cancel()` | 强制终止 |
| `stream drop` | 软取消（graceful shutdown） |
| 两者都 drop | 立即取消 |

Executor 通过 mpsc channel 检测关闭 → 终止执行。

### 6C 错误三分法

```rust
enum GraphError {
    Terminal(TerminalError),      // 终止执行，stream 关闭
    Recoverable(RecoverableError), // 内部重试 / fallback，stream 继续
    Observed(ObservedError),      // 仅事件，不影响 control flow
}
```

**为什么必须有 `Observed`：** 没有它，`Recoverable` 和 `Terminal` 之间会出现语义裂缝。

### 6D Handle + Channel

- Barrier decision channel：unbounded
- 反向压力边界：`max_active_barriers`（限制同时活跃的 Barrier 数量）

### 6E API 一致性

```rust
enum GraphEvent {
    NodeStart { node_id, span_id, step },
    NodeEnd { node_id, span_id },
    Node { event: NodeEvent },

    BarrierWaiting { barrier_id },
    BarrierResolved { barrier_id },

    ObservedError { error, node_id },

    GraphComplete { state, trace },
    GraphError { error: TerminalError, state },
}
```

Stream 包含一切信息。`GraphComplete` 和 `GraphError` 均携带最终 State。

---

## 7. GraphBuilder 契约

### 7A build() 返回 Result

```rust
fn build(self) -> Result<Graph, BuildError>
```

`BuildError` 仅验证结构完整性：

```rust
enum BuildError {
    DuplicateNode { id: NodeId },
    MissingNode { from: NodeId, to: NodeId },
    MissingEntryPoint,
    InvalidEdgeDefinition { from: NodeId, to: NodeId, reason: String },
}
```

**不管：** 循环、业务逻辑漏洞、运行时 unreachable。

### 7C Unrouted = Terminal Error

```rust
GraphError::Terminal(TerminalError::Unrouted {
    node: NodeId,
    state_snapshot: State,
    attempted_conditions: Vec<ConditionEval>,
})
```

- 不可隐式恢复
- 有 fallback edge → 先走 fallback
- 无 fallback → Terminal

### 7D 所有权

- build 后 `Graph` 不可变
- 要修改 → 重新 Builder
- 不可变 = 线程安全 = 可跨任务共享

---

## 8. 可观测性

### 8A 事件分级

单 channel + `EventLevel` metadata：

```rust
enum EventLevel {
    Graph,
    Node,
    Agent,
    Debug,
}
```

Consumer 按级别 filter：`subscribe(level <= Node)`。

### 8B TraceId + SpanId

```
TraceId  = 一次 Graph Execution
SpanId   = 一次 Node Execution
iteration = Agent 内部轮次
```

AgentNode 共享一个 SpanId，内部轮次通过 `iteration` 字段区分。

### 8C 双写模型

```
GraphExecutor
   ├── EventStream (实时 UI / 控制)
   └── tracing::Span (可观测性 / 运维)
```

- EventStream ≠ logging
- 内建 `tracing` crate 集成，结构化输出

### 8D ExecutionTrace

```rust
struct ExecutionTrace {
    trace_id: TraceId,
    initial_state: State,
    entries: Vec<ExecutionEntry>,
    barrier_decisions: Vec<BarrierDecisionRecord>,
    edge_evaluations: Vec<EdgeEvalRecord>, // 确保 replay deterministic
    end_event: GraphTerminalEvent,
}
```

**为什么必须记录 `edge_evaluations`：** 没有它，replay 时 graph routing 不可复现。

---

## 9. Checkpoint + Resume + Fork（v0.3 预留）

### 9B Checkpoint 内容

```rust
struct Checkpoint {
    checkpoint_id: CheckpointId,
    trace_id: TraceId,
    span_id: SpanId,
    step: u32,
    state: State,          // 完整快照
    current_node: NodeId,
    metadata: CheckpointMetadata,
}
```

- 保存完整 State（非 Delta）
- `graph_hash` 确保图结构可校验

### 9C Resume 语义

- **新 trace_id，关联原 trace_id**（`original_trace_id` + `resumed_from` + `resume_count`）
- **图可以变**，校验 `graph_hash`，变了就 warn 不断
- **Step 计数器从 checkpoint 继续累加**，不重置（防止无限 resume 绕过 max_steps）

### 9D Fork

- 深拷贝 State，共享 Graph
- 可并发执行（独立 executor + stream + handle）
- Fork trace 关联 parent trace

### 9E 存储层抽象

```rust
trait CheckpointStore {
    async fn save(&self, cp: &Checkpoint) -> Result<CheckpointId>;
    async fn load(&self, id: CheckpointId) -> Result<Checkpoint>;
    async fn list(&self, trace_id: TraceId) -> Result<Vec<Checkpoint>>;
    async fn delete(&self, id: CheckpointId) -> Result<()>;
}
```

v0.3 提供 `MemoryStore` + `FileStore`。Redis 放 v0.4+。

### 9A Checkpoint 粒度（待定）

待讨论：Per-Node / On-Demand / Barrier-Only / 混合模式。

---

## 优先级排序

| 议题 | 优先级 | 状态 |
|------|--------|------|
| 有环图 + max_steps | P0 | 已确定 |
| AgentNode 黑盒 ReAct | P0 | 已确定 |
| Barrier 决策缓存 Bug | P0 | 已修 |
| EdgeVisits + Goto Bug | P0 | 已修 |
| Edge 三层语义拆分 | P0 | 待落地 |
| StateExt getter | P1 | 进行中 |
| ReducerRegistry 预留 | P1 | 待设计 |
| BuildError Result | P1 | 待落地 |
| Unrouted = Terminal | P1 | 待落地 |
| 错误三分法 | P1 | 待落地 |
| GraphBuilder 双模式 | P2 | 待定 |
| TypedState / Schema | P3 | 太早 |
| ParallelNode + StatePatch | P3 | 太早 |
| Checkpoint + Resume | P3 | 预留接口 |
| Agent Hook | P3 | v0.3+ |

---

## 系统级定性

> LeLLM Graph 正在从 "Graph execution engine" 进化为
> **deterministic replayable event-driven computation system**
