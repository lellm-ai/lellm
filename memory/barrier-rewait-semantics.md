---
name: barrier-rewait-semantics
description: Barrier 恢复时重新等待人类决策，decision 属于 Control Plane，不污染 State 或 Checkpoint
metadata:
  type: project
---

## 核心原则

Barrier decision 是 **Control Plane 命令**，不是 Data Plane 状态。

- 不放进 Checkpoint（Snapshot 不应该包含 Runtime Message）
- 不放进 MutationLog（decision 不一定导致 State 改变，不是业务状态）
- 恢复时 **Re-Wait**——重新等待人类决策，永远安全

## 恢复流程

```
Checkpoint → current = Barrier
        ↓
重新创建 GraphHandle + decision_rx
        ↓
发送 GraphEvent::AwaitingBarrier { barrier_id, restored: true }
        ↓
UI 提示："工作流已从检查点恢复，正在重新等待人工决策。"
```

## 危险模式

**绝对不要**把 `approved=true` 写进 State 然后让 Barrier 自动跳过——这绕过了 Human-in-the-loop。

## occurrence 计数器

当前 `barrier_node.rs:109` 硬编码 `occurrence=0`，需要修复。Engine 应维护 per-node 计数器。

## 未来的 Command Log

v0.5 不实现。但如果需要无人值守恢复，引入第四层：

```
Checkpoint + Mutation WAL + Command Log
```

Command Log 记录 Approve/Reject/Resume/Cancel/Retry 等控制命令。
与 Data WAL 职责分离。

相关链接：[[checkpoint-three-layer-architecture]], [[checkpoint-snapshot-wal]]
```
