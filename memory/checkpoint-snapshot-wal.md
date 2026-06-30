---
name: checkpoint-snapshot-wal
description: Checkpoint 采用 Snapshot + WAL 模型，Snapshot 快速恢复，WAL Replay 几十条，MutationLog 保留完整审计
metadata:
  type: project
---

## Snapshot + WAL 模型

Checkpoint = Snapshot(State) + WAL(Vec<Mutation> since snapshot)

恢复 = 加载 Snapshot → Replay 几十条 Mutations（不是几百万条）

与数据库经典模型对齐：PostgreSQL base backup + WAL segments；RocksDB Snapshot + MemTable。

## MutationLog vs WAL

- **WAL**（Checkpoint 内部）：since snapshot，可截断，只用于恢复
- **MutationLog**（独立存储）：append-only，完整审计历史，不参与恢复路径

## 四层数据模型

1. **AgentState** — 执行器工作集（当前 Prompt Buffer）
2. **Checkpoint** — 工作集快照（Snapshot + WAL）
3. **MutationLog** — 工作集 WAL（Runtime 如何演化）
4. **Conversation Archive** — 产品数据资产（所有 Message、Tool Call、Token）

恢复目标：快速恢复工作集，不是重建整个历史。

相关链接：[[checkpoint-three-layer-architecture]], [[agent-state-working-set]]
