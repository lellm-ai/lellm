---
name: commit-batch-pipeline
description: commit() 拆分为 take_commit_batch() + 分发 + apply(batch) 流水线，CommitBatch 作为分发枢纽
metadata:
  type: project
---

## 核心原则

commit() 做了两件事（take + apply），随着消费者增多会越来越胖。
拆分为三原语：`record()` / `take_commit_batch()` / `apply(batch)`。

## CommitBatch

```rust
pub struct CommitBatch<M> {
    pub step: usize,
    pub node_id: NodeId,
    pub started_at: Instant,
    pub finished_at: Instant,
    pub mutations: Vec<M>,
}
```

## 流水线

```
LeafNode → record() → Pending Mutations
        ↓
take_commit_batch()
        ↓
├── TraceRecorder    (session 调试，执行结束释放)
├── MutationLog      (持久化 WAL，落盘)
├── Metrics
├── CheckpointTrigger
│
└── apply(batch) → State
```

## ExecutionTrace vs MutationLog

**ExecutionTrace** = 一次执行 session 的调试信息（step, node, duration, mutations）
**MutationLog** = 持久化的 WAL（sequence, timestamp, mutation, metadata）

前者 session 结束就释放，后者落盘持久化。职责不同。

## Engine API

只保留：`record(...)` / `take_commit_batch()` / `apply(...)`
没有 `commit()`，没有 Trace/Checkpoint/Metrics 的耦合。

相关链接：[[checkpoint-three-layer-architecture]], [[agent-state-working-set]]