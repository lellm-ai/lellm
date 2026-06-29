# v0.4 Runtime Architecture — Plan vs Reality 对照表

> 日期：2026-06-29 | 目的：修正 v04-architecture-evolution.md 中过时的"待完成"项
>
> 基于代码实际状态的全面审计。

---

## 总览

| Phase | Plan 描述 | 实际状态 | 差距 |
|-------|-----------|----------|------|
| Phase 1 — ExecutionEngine | "新增" | ✅ 已存在 | Plan 过时 |
| Phase 2 — 删除 BranchState | "待完成" | ✅ 已删除 | Plan 过时 |
| Phase 3 — Node API 统一 | "待完成" | ⚠️ 部分完成 | LeafNode 已有，FlowNode 待迁移 |
| Phase 4 — Composite Node | "两阶段迁移" | ⚠️ 部分完成 | ParallelNode 已在 ExecutorOperation |
| Phase 5 — Streaming | "新增 run_inline_stream" | ❌ 未开始 | run_inline_stream 不存在 |
| Phase 6 — LlmInvoker | "新增" | ✅ 已完成 | Plan 过时 |
| Phase 7 — Checkpoint | "新增架构" | ⚠️ 架构完成，未集成 | 基础设施有了，execution_loop 未接入 |
| Phase 8 — ExecutionTrace | "新增" | ⚠️ 类型已定义，未集成 | trace.rs 存在，execution_loop 未接入 |

**结论：** Plan 约 60% 的工作已经做了，但文档没有更新。

---

## Phase 1 — ExecutionEngine

### Plan 描述

> 新增 `ExecutionEngine<S>`
>
> 初始字段：state, stream, cancel, control, metadata, mutations, flow_events

### 实际状态

**文件：** `lellm-graph/src/node_context.rs:156-171`

```rust
pub struct ExecutionEngine<S: WorkflowState> {
    state: S,
    stream: Option<Arc<dyn StreamSink>>,
    cancel: CancellationToken,
    control: ExecutionControl,
    metadata: NodeMetadata,
    mutations: Vec<S::Mutation>,
    flow_events: Vec<FlowEvent>,
}
```

**完全一致。** 此外还实现了：

- `ExecutorState<S>` trait（`build_node_context`, `clone_state`, `replace_state`, `apply_batch`, `take_control`, `take_metadata`, `take_flow_events`, `emit_flow_event`）
- `ExecutionView<S>` trait（`state`, `emit`, `is_cancelled`）
- `commit()` — Unit of Work
- `ExecutionContext<S>` 向后兼容别名

### 问题

1. **定义位置不佳：** `ExecutionEngine` 定义在 `node_context.rs` 中（575 行），文件名为"node_context"但实际包含了 ExecutionEngine、ExecutorState、ExecutionView、LeafContext、NodeContext、ExecutionControl、NodeMetadata、ExecutionSignal、NextAction。这是 **晦涩性** 坏味道。

2. **`spawn_child()` 只存在于注释中：** 第 154 行注释提到 `spawn_child()`，但方法不存在。ParallelNode 直接 `ExecutionEngine::new()` 创建子 engine。

### 建议

- [ ] 将 `ExecutionEngine` + `ExecutorState` + `ExecutionView` 拆分为 `execution_engine.rs`
- [ ] `node_context.rs` 只保留 `LeafContext` + `NodeContext`
- [ ] 删除 `spawn_child()` 注释或实现它

---

## Phase 2 — 删除 BranchState

### Plan 描述

> 删除：BranchState, Overlay, ChangeLog Merge, ReducerRegistry

### 实际状态

**已完全删除。** grep 结果：

- `BranchState` — 只在 `workflow_state.rs:122` 的注释中提到（职责边界文档）
- `Overlay` — 同上
- `ReducerRegistry` — 代码中不存在
- `branch_state.rs` — 文件不存在

文档中 ADR 归档（第 1027-1028 行）也确认了：

```
| 2 | 删除 BranchState | ✅ | branch_state.rs 已删除 |
| 3 | 删除 delta.rs + ReducerRegistry | ✅ | delta.rs 已删除 |
```

### 建议

- [ ] 更新 Plan 主文，将 Phase 2 标记为已完成
- [ ] 清理 `workflow_state.rs:122` 中提及 BranchState/Overlay/ChangeLog 的注释

---

## Phase 3 — Node API 统一

### Plan 描述

> 统一 `trait FlowNode { async fn execute(&self, ctx: &mut NodeContext<'_>) }`
> 删除 execute_stream()
> 删除 state_mut()

### 实际状态

**三层 trait 并存：**

| Trait | Context | 状态 | 使用者 |
|-------|---------|------|--------|
| `LeafNode<S>` | `LeafContext` (只读 `&S`) | ✅ 活跃 | LLMNode, ToolNode, PostLLMGuard, CompactorNode, BudgetCondition |
| `ExecutorOperation<S>` | `&mut ExecutionEngine<S>` | ✅ 活跃 | ParallelNode |
| `FlowNode<S>` | `NodeContext` (可变 `&mut S`) | ⚠️ 向后兼容 | TaskNode, ConditionNode, BarrierNode, External, ParallelNode branches |

**关键发现：**

1. **LeafNode 已存在且工作正常。** ReAct 的所有节点已迁移为 LeafNode。

2. **FlowNode 仍然存在且被广泛使用：**
   - `TaskNode` — 用户自定义回调，使用 `NodeContext`
   - `ConditionNode` — 条件路由，使用 `NodeContext`
   - `BarrierNode` — 审批屏障，使用 `NodeContext`
   - `ParallelNode` 的 branches — `Vec<(String, Arc<dyn FlowNode<S>>)>`
   - `NodeKind::External` — 外部节点绑定

3. **NodeContext 没有 `state_mut()` 公开方法。** 但内部持有 `&'a mut S`，通过 `replace_state()` 可修改。所以"删除 state_mut()"的说法不准确——应该说是"LeafContext 没有，NodeContext 有"。

4. **`execute_stream()` 不存在于 Node trait 中。** 但 `ToolUseLoop::execute_stream()` 仍然存在（`runtime.rs:221`），它是 Agent 层的 API，返回 `AgentStream`。

### 未完成的迁移

以下节点仍实现 `FlowNode`（使用 `NodeContext`），未迁移为 `LeafNode`：

- [ ] `TaskNode` — 用户回调签名依赖 `NodeContext`
- [ ] `ConditionNode` — 只读 state + goto/end，可以迁移
- [ ] `BarrierNode` — 只读 state + pause，可以迁移
- [ ] `AgentFlowNode` — 在 agent 层，需要确认

### 建议

- [ ] 将 `ConditionNode` 迁移为 `LeafNode`（只读 state + goto，不需要写）
- [ ] 将 `BarrierNode` 迁移为 `LeafNode`（pause 是控制信号，不写 state）
- [ ] `TaskNode` 保留 `FlowNode`（用户回调需要灵活性），或新增 `LeafTaskNode`
- [ ] Plan 应明确"FlowNode 标记为 `#[deprecated]`，新代码使用 LeafNode"

---

## Phase 4 — Composite Node / ExecutorOperation

### Plan 描述

> 第一阶段：ParallelNode 可继续实现 FlowNode
> 第二阶段：迁移到 ExecutorOperation

### 实际状态

**ParallelNode 已经实现 `ExecutorOperation<S>`。** 见 `parallel_node.rs:200-340`。

执行流程完全符合 Plan 描述：

```
clone base state → fork cancel + stream → futures::join_all
→ commit mutations → MergeStrategy::merge → replace_state
```

**但 ParallelNode 的 branches 仍然是 `Arc<dyn FlowNode<S>>`，不是 `LeafNode`。**

### ExecutorState trait 的问题

`ExecutorState<S>` trait 定义了 Composite 节点需要的完整能力：

```rust
pub trait ExecutorState<S>: ExecutionView<S> {
    fn build_node_context(&mut self) -> NodeContext<'_, S>;
    fn clone_state(&self) -> S;
    fn replace_state(&mut self, state: S);
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = S::Mutation>);
    fn take_control(&mut self) -> (NextAction, Option<ExecutionSignal>);
    fn take_metadata(&mut self) -> NodeMetadata;
    fn take_flow_events(&mut self) -> Vec<FlowEvent>;
    fn emit_flow_event(&mut self, event: FlowEvent);
}
```

但此 trait **不是 dyn compatible**（注释已说明），因为：
- `build_node_context()` 返回生命周期绑定的 `NodeContext`
- `apply_batch()` 使用 `impl IntoIterator`

所以 `ExecutorOperation` 直接接收 `&mut ExecutionEngine<S>`，不经过 `ExecutorState` trait 分发。

### 建议

- [ ] Plan 更新：ParallelNode 已在 ExecutorOperation 上
- [ ] 考虑将 ParallelNode 的 branches 改为 `Arc<dyn LeafNode<S>>`，或保留 FlowNode 以兼容 TaskNode 等

---

## Phase 5 — Streaming 统一

### Plan 描述

> 新增 `run_inline_stream()`
> Graph 为唯一执行路径
> Agent Runtime 仅负责 StreamChunk → Adapter → AgentEvent

### 实际状态

**`run_inline_stream()` 不存在。** 当前有：

| API | 位置 | 状态 |
|-----|------|------|
| `Graph::run_inline(&self, exec_ctx, max_steps)` | `graph.rs:266` | ✅ 存在，无 event/channel |
| `run_execution_loop(graph, state, ...)` | `execution_loop.rs:37` | ✅ 存在，有 event_tx + decision_rx + cancel_rx |
| `ToolUseLoop::execute()` | `runtime.rs` | ✅ 存在，内部 build ReAct Graph + run_inline |
| `ToolUseLoop::execute_stream()` | `runtime.rs:221` | ✅ 存在，返回 AgentStream (mpsc receiver) |

**差距分析：**

1. `Graph::run_inline()` 接收 `&mut ExecutionEngine<S>`，不发射 GraphEvent。适合嵌套调用（如 AgentFlowNode）。

2. `run_execution_loop()` 是完整执行路径，发射 GraphEvent，支持 Barrier。在 `tokio::spawn` 中运行。

3. **没有 `run_inline_stream()` 这样的 API。** Plan 提到的"统一流式与阻塞"没有实现。

4. `ToolUseLoop::execute_stream()` 返回 `AgentStream`（`mpsc::Receiver<AgentEvent>`），内部使用 `AgentEventSink` 实现 `StreamSink`。这个适配层是存在的。

### AgentEvent 的去留

Plan 说"AgentEvent 消失"，但实际：

- `AgentEvent` 仍然存在（`lellm-agent/src/runtime/event.rs`）
- `AgentEventSink` 实现 `StreamSink`，将 `StreamChunk` 转换为 `AgentEvent`
- 这不是"俄罗斯套娃"，而是合理的适配层

### 建议

- [ ] 明确 `run_inline_stream()` 的签名（如果还需要）
- [ ] 或者确认现有两层 API（run_inline + run_execution_loop）已足够
- [ ] AgentEvent 保留作为 Agent 层的领域事件，Plan 中"AgentEvent 消失"应修正

---

## Phase 6 — LlmInvoker

### Plan 描述

> 新增 LlmInvoker
> 职责：Retry, Fallback, Stream State Machine

### 实际状态

**完全存在。** `lellm-agent/src/runtime/invoker.rs` (309 行)

```rust
pub struct LlmInvoker {
    model: ResolvedModel,
    fallback: Arc<dyn FallbackStrategy>,
    max_retries: usize,
    backoff: BackoffStrategy,
}
```

具备：
- ✅ Retry（retry before first chunk）
- ✅ Fallback（FallbackStrategy trait）
- ✅ Stream State Machine（NotStarted → HeadersReceived → FirstChunkSent → Finished）
- ✅ RetryAwareStream（has_sent_data 门控）
- ✅ `invoke()` — 阻塞调用
- ✅ `invoke_stream()` — 流式调用

分层完全符合 Plan：

```
LLMNode → LlmInvoker → LlmProvider → HTTP Client
```

### 建议

- [ ] Plan 标记为已完成

---

## Phase 7 — Checkpoint Infrastructure

### Plan 描述

> 新增 CheckpointCodec, CheckpointBlob, CheckpointStore

### 实际状态

**分层架构已完全实现：**

```
Checkpoint<S> (typed snapshot)
  → CheckpointCodec<S> (serialize/deserialize)
    → CheckpointBlob (bytes + metadata)
      → BlobCheckpointStore (bytes in/out SPI)
        → InMemoryBlobStore (HashMap backend)
```

文件：
- `checkpoint.rs` (135 行) — `Checkpoint<S>`, `CheckpointPolicy`
- `checkpoint_codec.rs` (144 行) — `CheckpointCodec<S>`, `SerdeCheckpointCodec<S>`, `TypedCheckpointStore`
- `store.rs` (179 行) — `CheckpointBlob`, `BlobCheckpointStore`, `InMemoryBlobStore`

**但是：**

1. **`run_execution_loop` 没有调用 `save_checkpoint()`。** CheckpointPolicy 定义了但没被 wired in。

2. **`run_inline` 也没有 Checkpoint 支持。**

3. **恢复逻辑不存在。** 没有 `load checkpoint → restore state → resume from node` 的路径。

### 缺失的工作

```
CheckpointPolicy: EveryNode, BarrierOnly, Manual
```

需要实现：

1. ExecutionEngine 中注入 `Option<TypedCheckpointStore<S>>`
2. execution_loop 中，根据 policy 在 commit() 后调用 save
3. 恢复路径：接受 `Option<Checkpoint<S>>` 作为起点
4. Barrier 恢复：decision queue 重建

### 建议

- [ ] Plan 区分"Checkpoint 基础设施"（已完成）和"Checkpoint 集成"（未开始）
- [ ] 恢复路径的详细设计单独作为 Phase 7b

---

## Phase 8 — ExecutionTrace

### Plan 描述

> 新增 ExecutionTrace, TraceStep
> 仅用于 Debug, Replay, Observability
> Checkpoint 不依赖 Trace
> Engine 执行结束后返回 Trace

### 实际状态

**类型已定义。** `lellm-graph/src/trace.rs` (131 行)

```rust
pub struct TraceStep<E> { step, node_id, mutations }
pub struct ExecutionTrace<E> { steps: Vec<TraceStep<E>> }
pub trait TraceSink<E> { fn record_step(&mut self, step: TraceStep<E>) }
pub struct MemoryTraceSink<E> { trace }
pub struct ExportedTrace { steps: Vec<ExportedTraceStep> } // JSON export
```

**但是：**

1. **`execution_loop` 没有使用 TraceSink。** 没有 `record_step()` 调用。

2. **`ExecutionEngine` 没有 TraceSink 字段。**

3. **`execution_log: Vec<ExecutionEntry>` 存在**，但这是 `GraphResult` 的一部分，不是 `ExecutionTrace`。两者是并行的审计机制。

### 双重审计机制的问题

当前有两条审计路径：

| 机制 | 位置 | 内容 | 状态 |
|------|------|------|------|
| `execution_log` | `execution_loop.rs` | step, node_name, time, success | ✅ 在用 |
| `ExecutionTrace` | `trace.rs` | step, node_id, mutations | ❌ 未接入 |

**它们的关系是什么？** `execution_log` 记录执行元数据（时间、成功与否），`ExecutionTrace` 记录 Mutation 审计（什么变更了 State）。两者互补，但 Plan 没有说明。

### 内存问题

`ExecutionTrace<E>` 是无上界的 `Vec<TraceStep<E>>`。长运行场景（如 1000+ 步的 Agent loop）会积累大量 Mutation 记录。

Plan 没有提到：
- 容量上限
- 磁盘溢出
- 实时消费

### 建议

- [ ] 明确 `execution_log` 与 `ExecutionTrace` 的分工
- [ ] 将 TraceSink 接入 execution_loop
- [ ] 考虑 Trace 容量策略（ring buffer？采样？）
- [ ] Plan 补充"Trace 与 Checkpoint 的关系"——当前设计是独立的，这是正确的

---

## 其他发现的问题

### 1. `NodeContext` 仍持有 `&mut S`

Plan 说"删除 state_mut()"，但 `NodeContext` 内部持有 `&'a mut S`，并通过 `replace_state()` 暴露写能力。

**这不是问题**——`replace_state()` 是组合节点（如 ParallelNode 子分支）需要的 sanctioned API。关键是普通节点不直接调用它。

但 Plan 的描述不准确：不是"删除 state_mut()"，而是"LeafContext 不提供写能力，NodeContext 保留 replace_state()"。

### 2. `graph.rs` 534 行 — 接近上限

`graph.rs` 包含：
- `Graph<S, M>` struct
- `GraphBuilder` DSL
- `PendingEdge`
- `run_inline()`
- `run_streaming()` (如果有的话)

534 行接近 Rust 400 行建议上限。考虑拆分：
- `graph.rs` — Graph struct + 基础方法
- `graph_builder.rs` — GraphBuilder DSL + PendingEdge
- （`execution_loop.rs` 已独立）

实际上 `react/` 下已有 `graph_builder.rs`（Agent 层），graph 层没有。

### 3. `execution_loop.rs` 469 行 — 超过上限

包含：
- `run_execution_loop()` — 主循环
- `wait_for_barrier_decision()` — Barrier 等待
- `send_complete()` — 完成事件
- `apply_barrier_decision()` / `apply_barrier_decision_generic()` — Barrier 决策应用

建议拆分：
- `execution_loop.rs` — 主循环
- `barrier_wait.rs` — Barrier 等待 + 决策应用

### 4. `node_context.rs` 575 行 — 严重超过上限

如 Phase 1 所述，应拆分。

---

## 文件行数汇总

| 文件 | 行数 | 状态 | 建议 |
|------|------|------|------|
| `node_context.rs` | 575 | ❌ 超标 | 拆分为 execution_engine.rs + node_context.rs |
| `execution_loop.rs` | 469 | ❌ 超标 | 拆出 barrier_wait.rs |
| `graph.rs` | 534 | ⚠️ 接近 | 拆出 graph_builder.rs |
| `parallel_node.rs` | 340 | ✅ OK | — |
| `workflow_state.rs` | 192 | ✅ OK | — |
| `node.rs` | 232 | ✅ OK | — |
| `checkpoint.rs` | 135 | ✅ OK | — |
| `checkpoint_codec.rs` | 144 | ✅ OK | — |
| `store.rs` | 179 | ✅ OK | — |
| `trace.rs` | 131 | ✅ OK | — |
| `invoker.rs` (agent) | 309 | ✅ OK | — |
| `typed_state.rs` (agent) | 234 | ✅ OK | — |
| `llm_node.rs` (agent) | 197 | ✅ OK | — |
| `tool_node.rs` (agent) | 209 | ✅ OK | — |
| `guards.rs` (agent) | 239 | ✅ OK | — |

---

## 修正后的 Phase 状态

| Phase | 修正后状态 | 剩余工作 |
|-------|-----------|----------|
| Phase 1 — ExecutionEngine | ✅ 已完成 | 拆分 node_context.rs |
| Phase 2 — 删除 BranchState | ✅ 已完成 | 清理残留注释 |
| Phase 3 — Node API | ⚠️ 60% | ConditionNode, BarrierNode → LeafNode |
| Phase 4 — Composite Node | ✅ 已完成 | Parallel branches 考虑 LeafNode |
| Phase 5 — Streaming | ❌ 0% | 确认 API 需求，可能不需要 run_inline_stream |
| Phase 6 — LlmInvoker | ✅ 已完成 | 无 |
| Phase 7 — Checkpoint | ⚠️ 50% | 架构完成，集成未开始 |
| Phase 8 — ExecutionTrace | ⚠️ 30% | 类型完成，集成未开始 |

**总体进度：约 55-60%**
