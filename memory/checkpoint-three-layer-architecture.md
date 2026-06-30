---
name: checkpoint-three-layer-architecture
description: Checkpoint 分层为 Trigger/Retention/Store，MutationLogStore 独立，不依赖 Agent 语义
metadata:
  type: project
---

## 三层架构

### 第一层：CheckpointTrigger（何时保存）

```rust
pub struct CheckpointTrigger {
    pub every_commits: Option<NonZeroUsize>,
    pub every_duration: Option<Duration>,
    pub on_barrier: bool,
    pub manual_only: bool,
}

pub struct CheckpointStats {
    commits_since_last: usize,
    last_checkpoint: Instant,
}

pub enum CheckpointEvent {
    Commit,
    BarrierResolved,
    Manual,
    // 可扩展: SubgraphFinished, LoopFinished, RetrySucceeded
}
```

`should_checkpoint(&stats, event) -> bool` — Engine 只问这一句。

**关键原则：** Trigger 只依赖 Engine 语义（commit count, time, barrier），不依赖 Agent 语义（token, cost, provider）。如需基于 Token 触发，Agent 层调 `engine.request_checkpoint()` 即可。

### 第二层：CheckpointRetention（保留策略）

```rust
pub struct CheckpointRetention {
    pub keep_latest: usize,
    pub max_age: Option<Duration>,
}
```

与 Trigger 完全无关。每 100 commits 保存 + 只保留最近 3 个，是两者的组合。

### 第三层：CheckpointStore（持久化）

Store 只管 save/load Blob，不管生命周期管理。

### 独立线：MutationLogStore

Append-only，完全不属于 Checkpoint 配置。参见 [[checkpoint-snapshot-wal]] 和 [[v04-progress-status]]。

## 配置终态

```rust
pub struct CheckpointConfig {
    pub trigger: CheckpointTrigger,
    pub retention: CheckpointRetention,
}
```

MutationLogStore 独立配置，不在此处。
