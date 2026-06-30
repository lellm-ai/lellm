---
name: agent-state-working-set
description: AgentState 是执行器工作集，不是对话档案；四层模型区分 Runtime/Checkpoint/MutationLog/Archive
metadata:
  type: project
---

## 核心区分

**AgentState = Runtime Working Set**（必须足够继续执行，不保证完整历史）

不是 Conversation Archive。

## 四层模型

### 1. AgentState — 执行器工作集
- 当前 Prompt Buffer（40-80 条消息）
- System + Summary + 最近 N 条 + Memory References
- Compactor 负责保持可控大小

### 2. Checkpoint — 工作集快照
- Snapshot(AgentState)，恢复执行
- 不负责历史

### 3. MutationLog — 工作集 WAL
- 记录 Runtime State **如何演化**（AppendMessage, Compact, ReplaceMessages...）
- 恢复后得到的是 **Compacted Runtime State**，不是原始历史
- 不保证完整聊天记录

### 4. Conversation Archive — 产品数据资产
- 所有 Message、Tool Call、Token、Reasoning
- 持久化存储（SQLite/ClickHouse/...）
- 供审计、分析、重放使用

## 与 Event Sourcing 的区别

传统 ES：Events → Replay → Current State（Events 是唯一真相）

我们的模型：Archive（完整历史）←→ Compactor ←→ Runtime State ←→ Checkpoint + WAL

**恢复目标：快速恢复工作集，不是重建整个历史。**

相关链接：[[checkpoint-three-layer-architecture]], [[checkpoint-snapshot-wal]]
