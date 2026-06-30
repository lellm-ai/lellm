# LeLLM v0.4 架构演进

> 版本：v0.4 | 日期：2026-06-29 | 状态：收尾完成（约 85%）
>
> 本文档记录 v0.3 → v0.4 的架构决策终态和关键 ADR。

## 目录

- [终态架构](#终态架构)
- [关键设计决策](#关键设计决策)
- [ADR 归档](#adr-归档)
- [Phase 状态](#phase-状态)
- [v0.5 路线图](#v05-路线图)

---

## 终态架构

```
Graph<S: WorkflowState>
  ├─ Edge<S>              — 条件闭包: &S -> bool
  ├─ NodeKind<S, M>
  │    ├─ Task(TaskNode<S>)                   — FlowNode（向后兼容）
  │    ├─ Condition(ConditionNode<S>)         — LeafNode + FlowNode 双 impl
  │    ├─ Barrier(BarrierNode<S>)             — LeafNode + FlowNode 双 impl
  │    ├─ Parallel(ParallelNode<S, M>)        — ExecutorOperation
  │    ├─ External(Arc<dyn FlowNode<S>>)      — 向后兼容
  │    └─ ExternalLeaf(Arc<dyn LeafNode<S>>)  — 声明式（推荐）
  │
  ├─ LeafContext<'a, S>     — 只读 &S + record(Mutation) + emit(StreamChunk)
  ├─ NodeContext<'a, S>     — 可变 &mut S + record + emit + replace_state
  │
  └─ ExecutionEngine<'a, S>  — 借用 State（&'a mut S）+ Mutation Buffer + Stream + Cancel
       ├─ build_leaf_context()  → LeafNode dispatch
       ├─ build_node_context()  → FlowNode dispatch (backward compat)
       ├─ commit()              → take_mutations → state.apply_batch()
       └─ CheckpointConfig      → callback-based save path

Agent Runtime
  └─ ReAct Graph<AgentState, AgentStateMerge>
       └─ LLMNode → ToolNode → PostLLMGuard → BudgetCondition → CompactorNode
            (全部 LeafNode, fully-qualified dispatch)

Checkpoint (Snapshot model)
  └─ Checkpoint<S> + graph_hash → CheckpointCodec → CheckpointBlob → BlobCheckpointStore
```

### 核心原则

| # | 原则 | 实现 |
|---|------|------|
| 1 | Graph 只编排 | Node Registry + Edge Routing + Condition Evaluation |
| 2 | ExecutionEngine 借用 State | `&'a mut S`，调用方持有所有权 + Mutation Buffer + Stream + Cancel |
| 3 | Leaf Node 只读 | `&S`，只能 `record(Mutation)` + `emit(StreamChunk)` |
| 4 | Composite Node 拥有执行 | `clone_state` / `replace_state` / `MergeStrategy` |
| 5 | Mutation 是唯一写入口 | 禁止 `ctx.state_mut()`，节点通过 `record()` 声明变更 |
| 6 | Checkpoint = Snapshot | "给我一个 Checkpoint 就能恢复"，非 Mutation Replay |
| 7 | graph_hash 是 correctness invariant | 加载时校验，不匹配拒绝恢复 |
| 8 | Stream 是 sink capability | `run_inline(state, Some(sink))` = 流式，`None` = 阻塞 |

---

## 关键设计决策

| # | 决策 | 结论 | 理由 |
|---|------|------|------|
| 1 | ReAct 建模粒度 | 中等（LLM + Tool + 条件边） | 可观测性与灵活性的平衡 |
| 2 | 事件体系 | RuntimeEvent + StreamChunk 分离 | Control Plane / Data Plane 解耦 |
| 3 | FlowNode 签名 | `execute(ctx) -> Result<(), GraphError>` | Context 驱动，零歧义 |
| 4 | State 模型 | Typed WorkflowState + Mutation | 编译期类型安全，零 JSON 开销 |
| 5 | 嵌套执行 | 内部不产生 RuntimeEvent | 防止递归嵌套和路径地狱 |
| 6 | Checkpoint 模式 | Snapshot（非 Mutation Replay） | 简单可靠，v0.5 再评估 |
| 7 | Message 存储 | Mutation 存完整 Message | 不引入 Message Store |
| 8 | TaskNode | 保留 FlowNode（向后兼容） | DSL 不能同时重构，v0.5 迁移 |
| 9 | run_inline_stream | 不需要 | `run_inline(state, Some(sink))` 已覆盖 |
| 10 | Condition/Barrier dispatch | fully-qualified syntax | `<Type as Trait>::execute()` 消除歧义 |

---

## ADR 归档

### ADR-0001: StreamSink — Producer Push

- `StreamSink::emit(chunk)` 同步、O(1)、无阻塞
- BufferedSink + Forward Task：Node → Buffer → Async Consumer
- 取消 = 消费者离开（不是背压）
- Token 流式只走 Stream，不写 State；流结束一次性 `record(AppendMessage)`

### ADR-0002: 统一执行路径 + LlmInvoker

- `ToolUseLoop` 内部构建 ReAct Graph，调用 `run_inline()`
- `execute()` 和 `execute_stream()` 共享同一 Graph 逻辑
- LlmInvoker 分层：`LLMNode → LlmInvoker → LlmProvider`
- Stream State Machine 决定 retry 边界：FirstChunkSent 后禁止重试

### ADR-0003: LeafContext / ExecutorOperation 分裂

- `LeafContext<'a, S>` — `&S` 只读，编译期保证不能修改 State
- `NodeContext<'a, S>` — `&mut S` 可变，向后兼容
- `ExecutorOperation` — `&mut ExecutionEngine`，Composite 节点专用
- Condition/Barrier 新增 LeafNode impl + fully-qualified dispatch

### ADR-0004: Checkpoint Correctness Invariant

- `CheckpointBlob.graph_hash` — 创建时写入，加载时校验
- `Graph::hash_u64()` — FNV hash of sorted node names + edge strings
- `CheckpointStoreError::GraphMismatch` — 不匹配时拒绝恢复
- CheckpointConfig 用 callback 而非泛型 trait object，避免 lifetime 地狱

---

## Phase 状态

| Phase | 描述 | 进度 | 关键变更 |
|-------|------|------|----------|
| 1 | ExecutionEngine | ✅ 100% | execution_engine.rs (340行) |
| 2 | 删除 BranchState | ✅ 100% | 清理过时注释 |
| 3 | Node API 统一 | ⚠️ 70% | Condition/Barrier LeafNode + fully-qualified dispatch |
| 4 | Composite Node | ✅ 100% | ParallelNode ExecutorOperation |
| 5 | Streaming | ❌ 不需要 | run_inline + sink 已覆盖 |
| 6 | LlmInvoker | ✅ 100% | Retry + Fallback + Stream SM |
| 7 | Checkpoint | ⚠️ 70% | 架构 + Save Path + graph_hash，Restore 留 v0.5 |
| 8 | ExecutionTrace | ⚠️ 30% | 类型已定义，接入留 v0.5 |

**总体进度：约 85%**（v0.4 scope 内）

### Wave 1-3 收尾记录

| Wave | 项目 | 提交 |
|------|------|------|
| 1 | graph_hash CheckpointBlob + node_context.rs 拆分 + 清理注释 | `3e39820` |
| 2 | Condition/Barrier LeafNode impl + fully-qualified dispatch | `25448d8` + `973cf1f` |
| 3 | CheckpointPolicy 集成 + barrier_wait.rs 拆分 | `61eb938` |

### 文件健康

| 文件 | 行数 | 状态 |
|------|------|------|
| `execution_engine.rs` | 340 | ✅ |
| `node_context.rs` | 251 | ✅ |
| `execution_loop.rs` | 330 | ✅ |
| `barrier_wait.rs` | 142 | ✅ |
| `graph.rs` | 544 | ⚠️ 接近 |
| `checkpoint.rs` | 160 | ✅ |
| 其余 | <400 | ✅ |

---

## v0.5 路线图

```
v0.5 (恢复 + 可观测 + 多智能体)
├─ Checkpoint 三层重构
│   ├─ CheckpointTrigger（every_commits, every_duration, on_barrier, manual）
│   ├─ CheckpointRetention（keep_latest, max_age）
│   └─ MutationLogStore（独立 append-only，不参与恢复路径）
├─ Checkpoint Restore 路径（Snapshot + WAL Replay）
├─ Barrier 恢复（Re-Wait 语义，不持久化 decision）
├─ Commit 流水线重构
│   ├─ commit() → take_commit_batch() + 分发 + apply(batch)
│   ├─ ExecutionTrace 接入（session 调试信息）
│   └─ MutationLog 接入（持久化 WAL）
├─ Stream 修复
│   ├─ StreamChunk::ThinkingDelta 增加 redacted 字段
│   └─ ProviderEvent → StreamChunk 转换提取到独立模块
├─ TaskNode → LeafTaskNode 迁移
├─ Graph::run_inline → Engine.run() 收敛
├─ Multi-Agent Orchestration
├─ Scheduler + Pause / Resume
└─ 确定性重放测试
```

### v0.4 Grill 决策纪要

| # | 决策 | 结论 |
|---|------|------|
| 1 | Checkpoint 分层 | Trigger / Retention / Store 三层，MutationLogStore 独立 |
| 2 | Barrier 恢复语义 | Re-Wait（重新等待人类决策），decision 属于 Control Plane |
| 3 | AgentState 定位 | 执行器工作集（working set），不是对话档案 |
| 4 | 四层数据模型 | Runtime State → Checkpoint → MutationLog → Conversation Archive |
| 5 | Commit 流水线 | 拆分为 take_commit_batch() + 分发 + apply(batch) |
| 6 | ExecutionTrace 定位 | session 调试信息，非 MutationLog 的内存版 |
| 7 | StreamChunk::ThinkingDelta | 增加 redacted 字段，禁止静默丢弃 |
| 8 | ProviderEvent 转换 | 提取到独立模块，不使用 Option<StreamChunk> |
