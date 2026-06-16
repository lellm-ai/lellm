---
name: parallel-state-delta-merge
description: ParallelNode 状态合并策略 - StateDelta + ReducerRegistry + 冲突默认报错
metadata:
  type: project
---

## 决策：Delta Merge 模型（方案 E/F）

**核心原则：节点产生 StateDelta（patch），不直接修改共享 State。**

### 为什么不是直接合并最终 State

两个分支各写 `"count": 2`，merge 时系统无法区分"两次 +1"还是"恰好写了相同值"——信息已丢失。

```
Fork → Branch A (delta: count += 1) → Merge
      → Branch B (delta: count += 1) → Apply Delta → count = 3
```

### Reducer 内置枚举

```rust
enum Reducer {
    Error,      // 默认 - 冲突即报错
    Replace,    // 最后写入者胜
    Append,     // 数组追加
    MergeObject, // 对象合并
    Sum,        // 数值求和
    Max,
    Min,
    Custom(Box<dyn Fn(...) + Send + Sync>),
}
```

### StateKey<T> 绑定 Reducer

Key、Type、Reducer 三者绑定，避免类型脱节：

```rust
pub static MESSAGES: StateKey<Vec<Message>> = 
    StateKey::new("messages", Reducer::Append);
```

### v0.3 优先级

见 [[memory:parallel-state-delta-merge]]
- **P0**: StateKey<T> ✅ 已完成
- **P1**: StateDelta — 节点输出 patch 而非突变 state
- **P2**: ParallelNode + ReducerRegistry

**关键洞察：State Merge 是前置条件，ParallelNode 不是。** 先有正确的合并语义，再做并行。
