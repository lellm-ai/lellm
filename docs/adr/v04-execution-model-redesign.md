# v0.4 执行模型重构设计决策

> 状态：Phase A, B, D, E 完成；Phase C 部分完成（2026-06-29，后续清理进行中）
> 日期：2026-06-29
> 来源：grilling session

---

## 背景

v0.4 执行模型重构的目标：砸碎 `HashMap<String, Value>` 硬编码，引入编译期类型安全的 `WorkflowState` + `Mutation` + `MergeStrategy` 框架，同时删除历史债务。

**总删除量**：~1700+ 行（executor.rs ~1170 + branch_state.rs ~180 + delta.rs ~340）

---

## 决策总览

| # | 决策 | 状态 | 说明 |
|---|------|------|------|
| 1 | 删除 GraphExecutor | ✅ 完成 | executor.rs 已删除 |
| 2 | 删除 BranchState | ✅ 完成 | branch_state.rs 已删除 |
| 3 | 删除 delta.rs + ReducerRegistry | ✅ 完成 | delta.rs 已删除 |
| 4 | Checkpoint 分层架构 | ✅ 完成 | 5 层解耦架构已实现，9 个新测试全部通过 |
| 5 | ExecutionEngine + ExecutorState | ✅ 完成 | 已实现，ExecutionContext 为 type alias |
| 6 | Executable 统一抽象 | ⚠️ 部分完成 | `emit_flow_event` 已加入 `ExecutorState`；`Executable` trait 因 Rust dyn compatibility 限制未能引入 |
| 7 | NodeContext 瘦身 | ✅ 完成 | 已删除 branch 字段，保留最小能力 |
| 8 | 测试迁移 | ✅ 完成 | SimpleExecutor 兼容层已实现，execution_loop 已拆分为独立模块 |

---

## 决策 1：删除 GraphExecutor ✅

**决定**：删除 `GraphExecutor` 及其所有关联代码。

**理由**：
- `GraphExecutor::run_loop()` 硬编码 `State`，无法用于 `Graph<AgentState>`
- 所有 Agent 场景走 `Graph::run_inline()`，不经过 `GraphExecutor`
- `_reducer_registry` 有下划线前缀——从未被使用

**影响**：
- `executor.rs` — ✅ 已删除
- `lib.rs` — ✅ 已删除 `pub use executor::GraphExecutor`

---

## 决策 2：删除 BranchState ✅

**决定**：彻底删除 `BranchState`、`ChangeRecord`、`ChangeOperation`。

**理由**：
- 三个原始职责已全部迁移：
  - overlay → `S::clone()`
  - ChangeLog → `ExecutionTrace`（未来）
  - merge → `MergeStrategy<S>`
- 没有 `FlowNode` 调用 `branch.get()` / `branch.set()` / `branch.changes()`

**连带删除**：
- `branch_state.rs` — ✅ 已删除
- `ExecutionContext::branch` 字段 — ✅ 已删除
- `NodeContext::branch` 字段 — ✅ 已删除
- `NodeContext::fork_branch()` — ✅ 已删除
- `WorkflowState::apply_branch_change()` — ✅ 已删除

---

## 决策 3：删除 delta.rs + ReducerRegistry ✅

**决定**：删除 `delta.rs`、`ReducerRegistry`、`StateDelta`、`DeltaOp`、`DeltaSource`、`Reducer`。

**理由**：
- `ReducerRegistry` 在 `GraphExecutor::run_loop()` 中被创建但从未使用
- Mutation 模型已经替代了 Delta 模型
- `StateMutation::Put/Delete` 比 `StateDelta::Put/Delete` 更类型安全

**影响**：
- `delta.rs` — ✅ 已删除
- `lib.rs` — ✅ 已删除 `pub use delta::*`

**残留项**：
- `state.rs` 的 `StateReducer` + `array_reducer` — `StateExt::reduce()` 和 `append_array()` 仍在使用，属于 State 的工具方法，保留
- `statekey.rs` 的 `Reducer` 枚举 — 是 `StateKey<T>` 的合并策略描述符，与 delta 层的 `Reducer` 不同，保留

---

## 决策 4：Checkpoint 分层架构 ✅ 已完成

**决定**：实施 Checkpoint 分层解耦架构（原计划推迟到 v0.5，已提前实施）。

**已实现的架构**：

```
Checkpoint<S>           ← Workflow 层，强类型，纯 Snapshot 模型 (checkpoint.rs)
       │
       ▼ serialize/deserialize
CheckpointCodec<S>      ← 序列化层 trait，对象 ↔ 二进制表示 (checkpoint_codec.rs)
       │
       ▼
CheckpointBlob           ← 跨 Codec 的统一载体 (checkpoint.rs)
       │
       ▼ save/load
BlobCheckpointStore      ← 存储层 SPI，bytes in / bytes out (store.rs)
       │
       ▼
InMemoryBlobStore        ← 内存后端实现 (store.rs)
```

**新增类型**：
- `CheckpointBlob` — 序列化后的二进制数据 + 元数据
- `CheckpointCodec<S>` — 序列化/反序列化 trait
- `SerdeCheckpointCodec<S>` — 默认 JSON 实现
- `BlobCheckpointStore` — bytes in / bytes out 存储 SPI
- `InMemoryBlobStore` — 内存后端（替代 `InMemoryCheckpointStore`）
- `TypedCheckpointStore` — Codec + BlobStore 组合，提供类型化的 save/load
- `CheckpointStoreError::Serialization` — 新增序列化错误变体

**删除/重命名**：
- `CheckpointStore` trait → `BlobCheckpointStore`（操作 CheckpointBlob）
- `InMemoryCheckpointStore` → `InMemoryBlobStore`

**测试**：
- `checkpoint_test.rs` — 6 个测试（Codec 序列化、Blob 结构、Store 操作、Error 变体、Policy 枚举）
- `checkpoint_restore_test.rs` — 3 个测试（完整恢复链路、load_latest、trace 隔离）

---

## 决策 5：ExecutionEngine + ExecutorState ✅

**决定**：引入 `ExecutionEngine<S>` 作为执行器内部对象，替代 `ExecutionContext<S>`。

### 当前实现

`ExecutionContext<S>` 是 `ExecutionEngine<S>` 的 **type alias**（向后兼容）：

```rust
pub type ExecutionContext<S> = ExecutionEngine<S>;
```

### ExecutionEngine（执行器）

```rust
pub struct ExecutionEngine<S: WorkflowState> {
    state: S,
    stream: Option<Box<dyn StreamSink>>,
    cancel: CancellationToken,
    control: ExecutionControl,
    metadata: NodeMetadata,
    mutations: Vec<S::Mutation>,
    flow_events: Vec<FlowEvent>,
}
```

**最小实现**——不预留 CheckpointManager、TraceRecorder、Scheduler 等未来组件。

### ExecutorState trait

```rust
pub trait ExecutionView<S: WorkflowState>: Send + Sync {
    fn state(&self) -> &S;
    fn emit(&self, chunk: StreamChunk);
    fn is_cancelled(&self) -> bool;
}

pub trait ExecutorState<S: WorkflowState>: ExecutionView<S> {
    fn build_node_context(&mut self) -> NodeContext<'_, S>;
    fn clone_state(&self) -> S;
    fn replace_state(&mut self, state: S);
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = S::Mutation>);
    fn take_control(&mut self) -> (NextAction, Option<ExecutionSignal>);
    fn take_metadata(&mut self) -> NodeMetadata;
    fn take_flow_events(&mut self) -> Vec<FlowEvent>;
}

impl<S: WorkflowState> ExecutorState<S> for ExecutionEngine<S> { ... }
```

`ExecutionView` 是受限视图（Leaf 节点不需要 `replace_state`）。
`ExecutorState` 是完整能力（Composite 节点使用）。

### run_inline() 签名

```rust
pub async fn run_inline(
    &self,
    exec_ctx: &mut ExecutionContext<S>,  // = ExecutionEngine<S>
    max_steps: usize,
) -> Result<(), GraphError>
```

内部循环：
1. `exec_ctx.build_node_context()` → `NodeContext<'_, S>`（能力视图）
2. `node.execute(&mut ctx).await` → 节点通过 `ctx.record()` 声明变更
3. `drop(ctx)` → 释放借用
4. `exec_ctx.take_mutations()` → 消费 Mutation 缓冲
5. `exec_ctx.state_mut().apply_batch(mutations)` → apply 到 State
6. `exec_ctx.take_control()` → 获取路由信号

---

## 决策 6：Executable 统一抽象 → ⚠️ 部分完成

**决定**：引入 `Executable<S>` trait 作为所有可执行对象的统一接口。

**当前状态**：部分实施。

### ✅ 已完成

1. **`emit_flow_event` 加入 `ExecutorState`** — Composite 节点（如 ParallelNode）可以通过 `ExecutorState` 发射 FlowEvent
2. **`ExecutorState` trait 完善** — 包含 `build_node_context`, `clone_state`, `replace_state`, `apply_batch`, `take_control`, `take_metadata`, `take_flow_events`, `emit_flow_event`

### ⚠️ 未完成的部分

**`Executable` trait 未能引入**，原因是 Rust 的 **dyn compatibility 限制**：

`ExecutorState<S>` trait 无法作为 `dyn ExecutorState<S>` 使用，因为：

1. **`build_node_context(&mut self) -> NodeContext<'_, S>`** — 返回类型中的生命周期 `'_'` 与 `&mut self` 绑定。Rust 不允许 dyn trait 的方法返回与自身生命周期绑定的引用（除非使用 explicit lifetime bound，如 `trait ExecutorState<'a, S>`，但这会让 API 变得极其复杂）。

2. **`apply_batch(&mut self, mutations: impl IntoIterator<Item = S::Mutation>)`** — `impl Trait` 在方法签名中等同于泛型类型参数，泛型方法破坏 dyn compatibility。

**尝试过的方案**：
- 方案 A：为 `ExecutorState` 添加生命周期参数 → 让 `Executable` 也变得复杂，需要传递生命周期
- 方案 B：将 `apply_batch` 改为 `Vec<S::Mutation>` → 解决了问题 2，但问题 1 仍然存在
- 方案 C：将 `build_node_context` 移到单独的 trait → `Executable` 无法构建 `NodeContext`，Leaf 节点无法工作

**结论**：在 Rust 当前类型系统下，`Executable<S>` + `dyn ExecutorState<S>` 的组合不可行。保持 `FlowNode<S>` 作为主要 trait，`ExecutorState<S>` 用于静态分发（Composite 节点内部使用）。

### 残留问题

- ~~`state_mut_ref()` 仍然存在于 `NodeContext`~~ → ✅ 已替换为 `replace_state()`（2026-06-29，sanctioned 组合节点 API，不暴露 `&mut S`）
- `NodeKind` 仍然是枚举（未简化为单一 `Executable` 变体）
- `run_inline()` 仍然通过 `FlowNode` 调用节点

**优先级**：中。当前代码能工作，这是架构优化。如果 Rust 未来支持 `RPITIT in traits`（Return Position Impl Trait In Traits）的 dyn compatibility，可以重新评估。

---

## 决策 7：NodeContext 瘦身 ✅

**决定**：`NodeContext` 只保留节点执行所需的最小能力。

### 当前实现

```rust
pub struct NodeContext<'a, S: WorkflowState = State> {
    state: &'a S,
    stream: Option<&'a dyn StreamSink>,
    cancel: &'a CancellationToken,
    control: &'a mut ExecutionControl,
    metadata: &'a mut NodeMetadata,
    mutations: &'a mut Vec<S::Mutation>,
    flow_events: &'a mut Vec<FlowEvent>,
}
```

**不提供**：
- ~~`state_mut()`~~ — 节点只能通过 `record()` 声明变更意图 ✅
- ~~`branch()` / `branch_mut()`~~ — 已删除 ✅
- ~~`fork_branch()`~~ — 已删除 ✅
- ~~`replace_state()`~~ — 这是 Executor 的能力，不是节点的能力 ✅
- ~~`consume_mutations()`~~ — 这是 Executor 的能力 ✅

**残留问题**：
- `state_mut_ref()` 当前返回 `&mut S`——被 ParallelNode 的 `FlowNode` 实现使用，用于合并子分支结果。由于 `ExecutorState` 不是 dyn compatible，ParallelNode 无法直接使用 `ExecutorState::replace_state()`。保留此方法直到 Rust 类型系统改进。

---

## 决策 8：测试迁移 → 🔴 阻塞（最高优先级）

**问题**：`SimpleExecutor` 随 `executor.rs` 一起被删除，但测试中仍有 ~39 处引用，导致 `cargo test --package lellm-graph` 编译失败。

**影响范围**：
- `tests/graph_test.rs` — ~30 处 `SimpleExecutor::default().execute(...)` 和 `.execute_stream(...)`
- `tests/parallel_test.rs` — ~9 处同上
- `tests/checkpoint_test.rs` — 已改为 `#[ignore]`
- `tests/checkpoint_restore_test.rs` — 已改为 `#[ignore]`

**迁移方案**：

`SimpleExecutor` 的旧 API：
```rust
let result = SimpleExecutor::default()
    .execute(Arc::new(graph), State::new())
    .await;
// result: GraphResult { state, execution_log, duration, trace_id }
```

新 API（基于 `run_inline`）：
```rust
let mut engine = ExecutionEngine::new(
    State::new(),
    None,
    CancellationToken::new(),
);
graph.run_inline(&mut engine, 100).await?;
// engine.state() → &State
// engine.into_state() → State
```

**差距**：`run_inline` 不返回 `GraphResult`（不含 `execution_log`、`trace_id`、`duration`）。

**解决方案**：在测试层包装一个 helper 函数：

```rust
/// 测试辅助 — 执行 Graph 并返回 GraphResult
async fn execute_graph(graph: &Graph) -> GraphResult {
    let start = Instant::now();
    let mut engine = ExecutionEngine::new(State::new(), None, CancellationToken::new());
    graph.run_inline(&mut engine, 100).await.expect("execution failed");
    let duration = start.elapsed();
    GraphResult {
        trace_id: TraceId::new(),
        state: engine.into_state(),
        execution_log: Vec::new(),
        duration,
    }
}
```

**注意**：`execute_stream`（流式执行 + `GraphHandle` + `GraphEvent`）目前没有被 `run_inline` 替代。这是 `GraphExecutor` 的核心功能（Barrier 支持、流式事件、Checkpoint 保存）。有两个选择：

1. **短期方案**：在 `graph.rs` 中实现 `execute_stream()` 方法，基于 `run_inline` 的增强版（在循环中发射 `GraphEvent`、处理 Barrier 信号）
2. **长期方案**：将流式执行逻辑提取为 `ExecutionEngine` 的方法

**建议**：采用短期方案。在 `Graph` 上实现 `execute_stream()`，内部是一个增强的执行循环（`run_inline` 的流式版本）。

---

## 执行计划

### Phase A：删除死代码 ✅ 完成

| 任务 | 文件 | 状态 |
|------|------|------|
| A1 | `executor.rs` | ✅ 已删除 |
| A2 | `branch_state.rs` | ✅ 已删除 |
| A3 | `delta.rs` | ✅ 已删除 |
| A4 | `lib.rs` | ✅ 已清理 module 声明 + re-export |
| A5 | `workflow_state.rs` | ✅ 已删除 `apply_branch_change()` |
| A6 | `state.rs` | ✅ 已删除 `apply_branch_change()` |

### Phase B：ExecutionEngine + ExecutorState ✅ 完成

| 任务 | 文件 | 状态 |
|------|------|------|
| B1 | `node_context.rs` | ✅ `ExecutionEngine` 已实现，branch 字段已删除 |
| B2 | `node_context.rs` | ✅ `ExecutionView` + `ExecutorState` traits 已定义 |
| B3 | `node_context.rs` | ✅ `ExecutionEngine` 实现 `ExecutorState` |
| B4 | `graph.rs` | ✅ `run_inline()` 使用 `ExecutionContext` (= `ExecutionEngine`) |
| B5 | `parallel_node.rs` | ✅ 分支执行使用 `ExecutionContext::new()` + `build_node_context()` |

### Phase C：Executable 统一抽象 ⚠️ 部分完成

| 任务 | 文件 | 操作 | 状态 |
|------|------|------|------|
| C1 | `node_context.rs` | `ExecutorState` 添加 `emit_flow_event` 方法 | ✅ |
| C2 | `node.rs` | 定义 `Executable` trait | ❌ 放弃（dyn compatibility 限制）|
| C3 | `node.rs` | 定义 `LeafAdapter` | ❌ 放弃（依赖 Executable）|
| C4 | `node.rs` | 简化 `NodeKind` | ❌ 放弃（依赖 Executable）|
| C5 | `parallel_node.rs` | 实现 `Executable`（不再实现 `FlowNode`）| ❌ 放弃（依赖 Executable）|
| C6 | `barrier_node.rs` | 实现 `Executable`（通过 LeafAdapter）| ❌ 放弃（依赖 Executable）|
| C7 | `graph.rs` | `run_inline` 循环改为调用 `Executable::execute()` | ❌ 放弃（依赖 Executable）|

**结论**：`ExecutorState` 因 Rust dyn compatibility 限制无法作为 trait object 使用（`build_node_context` 返回生命周期绑定的引用，`apply_batch` 使用泛型）。`Executable` trait 方案放弃。保持 `FlowNode` 作为主要接口。

**优先级**：中。当前代码能工作。如果 Rust 未来改进 dyn compatibility，可以重新评估。

### Phase D：Checkpoint 分层架构 ✅ 已完成

| 任务 | 文件 | 操作 | 状态 |
|------|------|------|------|
| D1 | `checkpoint.rs` | 引入 `CheckpointBlob` 统一载体 | ✅ |
| D2 | `checkpoint_codec.rs` | 定义 `CheckpointCodec<S>` trait | ✅ |
| D3 | `checkpoint_codec.rs` | 实现 `SerdeCheckpointCodec<S>`（默认 JSON） | ✅ |
| D4 | `store.rs` | `BlobCheckpointStore` trait（bytes in/bytes out） | ✅ |
| D5 | `store.rs` | `InMemoryBlobStore`（替代 InMemoryCheckpointStore） | ✅ |
| D6 | `checkpoint_codec.rs` | `TypedCheckpointStore`（Codec + BlobStore 组合） | ✅ |
| D7 | `checkpoint_test.rs` | 6 个新测试（Codec、Blob、Store、Error、Policy） | ✅ |
| D8 | `checkpoint_restore_test.rs` | 3 个新测试（恢复链路、load_latest、trace 隔离） | ✅ |
| D9 | `node_context.rs` | ExecutionEngine 集成 checkpoint 保存 | ⬜ 推迟 |
| D10 | `graph.rs` | run_inline 中集成 checkpoint | ⬜ 推迟 |

**说明**：D9-D10（ExecutionEngine 集成 checkpoint）推迟到后续迭代。当前 `TypedCheckpointStore` 提供类型化的 save/load API，调用方可在 `run_inline` 外部手动触发 checkpoint 保存。

### Phase E：测试迁移 ✅ 完成

| 任务 | 文件 | 操作 |
|------|------|------|
| E1 | `tests/graph_test.rs` | 定义 `execute_graph()` helper，替换所有 `SimpleExecutor` 引用 | ✅ |
| E2 | `tests/parallel_test.rs` | 同上 | ✅ |
| E3 | `test_executor.rs` | SimpleExecutor 兼容层（execute + execute_stream） | ✅ |
| E4 | `execution_loop.rs` | 流式执行循环从 test_executor 拆分到独立模块 | ✅ |
| E5 | 全量测试 | `cargo test --workspace` 通过 | ✅ |

---

## 已知问题与待决策

### 1. `state_mut_ref()` Hack

`NodeContext::state_mut_ref()` 当前返回 `&mut S`。
这是 ParallelNode 合并子分支结果时需要修改父 state 的入口。

**解决方案**：如果 Rust 未来支持 `ExecutorState` 作为 dyn trait，ParallelNode 将直接使用 `ExecutorState::replace_state()`，不再通过 `NodeContext` 修改 state。届时删除 `state_mut_ref()`。当前保留。

### 2. `GraphResult` 硬编码 `State`

`GraphResult` 的 `state` 字段类型为 `State`（非泛型）。
`GraphEvent::GraphComplete` 和 `GraphEvent::GraphError` 同理。

**解决方案**：流式执行（`execute_stream`）是 `GraphExecutor` 的遗留功能。v0.4 不做泛型化，v0.5 重构时一并处理。

### 3. ParallelNode 仍实现 FlowNode

当前 `ParallelNode` 实现 `FlowNode<S>`（通过 `NodeContext`），在内部创建 `ExecutionContext` 用于子分支。
这绕过了 `ExecutorState` trait。

**解决方案**：Phase C 中改为实现 `Executable<S>`，直接使用 `ExecutorState`。

### 4. `graph.rs` 注释引用 GraphExecutor ✅ 已修复

第 10 行注释已更新为：`运行时安全由 run_inline() 的 max_steps 参数负责`。

---

## 后续清理项

以下代码异味已识别，按优先级排列：

### 1. `BarrierNode` StateMutation 约束（中优先级）

`impl<S: WorkflowState<Mutation = StateMutation>> BarrierNode<S>` 约束了所有构造方法和 `apply_decision_to_ctx()`，这使得 `BarrierNode` 只能用于使用 `StateMutation` 的 state 类型。

**状态**：v0.5 待决策。当前没有自定义 state 类型需要使用 BarrierNode。

### 2. `GraphResult` / `GraphEvent` 泛型化（中优先级）

`GraphResult.state: State` 和 `GraphEvent::GraphError { state: State }` 硬编码了默认 `State` 类型，不支持 `Graph<AgentState>` 等 typed state 场景。

**状态**：v0.5 待决策。`SimpleExecutor` 本身就只支持 `Graph<State>`。

### 3. `instant_to_iso()` 日期计算粗糙（低优先级）

`state.rs` 中的 `instant_to_iso()` 使用粗略的月份/日期计算，跨年/跨月场景会产生错误日期。仅用于 Checkpoint 日志。

**建议**：用 `chrono` 或 `time` crate 替代。

### 4. `FlowEvent::Custom` Box<dyn Any>（低优先级）

`FlowEvent::Custom` 使用 `Box<dyn std::any::Any + Send + Sync>` 作为 payload，不可序列化。如果未来需要流式事件持久化，需重新设计。

### 5. 已完成的清理

- ~~`ParallelNodeBuilderWithMerge` 死代码~~ → ✅ 已删除（2026-06-29）
- ~~`test_executor.rs` 超长 (540 行)~~ → ✅ 拆分为 `test_executor.rs` (154 行) + `execution_loop.rs` (409 行)（2026-06-29）

---

## 风险与缓解

| 风险 | 缓解 |
|------|------|
| 测试不编译（SimpleExecutor 删除） | ✅ **已解决**，SimpleExecutor 兼容层 + execution_loop 模块 |
| 泛型 trait object 编译错误 | `ExecutorState<S>` 使用 `dyn`，确保 trait 是 object-safe |
| `Executable` 的 `&mut dyn ExecutorState` 借用问题 | `ExecutionEngine` 拥有 state，trait methods 只借用 `&mut self` |
| API break | `ExecutionContext` 保留为 type alias，`run_inline()` 签名不变 |
| ParallelNode 串行执行 | 当前实现就是串行（serial fallback），并行化是独立优化 |
| `execute_stream` 缺失 | ✅ **已解决**，execution_loop.rs 提供完整的流式执行循环 |

---

## 成功标准

1. `cargo build --workspace` 通过 ✅
2. `cargo test --workspace` 全部通过（≥ 127 测试）✅
3. `GraphExecutor`、`BranchState`、`delta.rs`、`ReducerRegistry` 从代码中消失 ✅
4. `ExecutionEngine` + `ExecutorState` 分离 ✅（`Executable` 因 dyn compatibility 限制放弃）
5. `NodeContext` 不提供 `state_mut()`、`replace_state()`、`fork_branch()` ✅（`state_mut_ref()` 保留供 ParallelNode 使用）
6. Checkpoint 分层解耦（Checkpoint / Codec / Blob / Store）✅ 已完成（9 个新测试）
7. 无新增 warning ✅
8. `test_executor.rs` 拆分为合理大小的模块 ✅（2026-06-29）
9. 死代码 `ParallelNodeBuilderWithMerge` 已清理 ✅（2026-06-29）
