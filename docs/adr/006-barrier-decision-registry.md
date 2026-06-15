# ADR-006: Barrier 决策采用 DecisionRegistry（Level-Triggered）

**日期：** 2026-06-15
**状态：** Accepted
**领域：** lellm-graph / BarrierNode / Human-in-the-loop

## 背景与问题

Barrier 决策通过 `GraphHandle::decide(barrier_id, decision)` 提交，由 executor 的 `wait_barrier_decision()` 接收。

**核心问题：决策顺序 ≠ Barrier 到达顺序。**

场景：Graph 有两个 Barrier（A → B），用户或外部 API 先提交了 B 的决策：

1. Barrier A 暂停，executor 等待 `barrier_id_A`
2. 用户先发送 `decide(barrier_id_B, Approve)` — 进入 channel
3. executor 收到 `barrier_id_B`，发现 `!= barrier_id_A`
4. **如果丢弃 → B 的决策永久丢失 → Barrier B 到达时永久阻塞**

## 设计原则

> **Barrier decisions are level-triggered, not edge-triggered.**
>
> 在 Barrier 进入等待状态之前提交的决策 MUST 被保留，
> 待 Barrier 到达时再取出应用。

这是 **correctness 要求**，不是性能优化。

## 方案对比

| 方案 | 优点 | 缺点 |
|------|------|------|
| A. 无缓存（丢弃不匹配） | 最简单 | **Bug** — 决策丢失 |
| B. Arc<Mutex<HashMap>> 共享 | 可被 Node 轮询 | 过度设计 — Node 从不读取 |
| C. DecisionRegistry（plain HashMap） | 轻量、无同步开销、职责清晰 | 仅 executor 可见 |

## 选定方案：C — DecisionRegistry

```rust
struct DecisionRegistry {
    pending: HashMap<BarrierId, BarrierDecision>,
}

impl DecisionRegistry {
    fn insert(&mut self, barrier_id: BarrierId, decision: BarrierDecision);
    fn take(&mut self, target_id: &BarrierId) -> Option<BarrierDecision>;
}
```

**wait_barrier_decision 流程：**

```
1. registry.take(&target_id)  — 先查缓存（命中则立即返回）
2. decision_rx.try_recv()     — drain channel 中已有的决策
3. 超时分支：timeout(recv) + 检查总超时 — 不匹配的入库
4. 无限等待：recv().await      — 不匹配的入库
```

**为什么不需要 Arc<Mutex<...>>：**
- 唯一消费者是 GraphExecutor 的 spawned task
- `GraphHandle::decide()` 通过 mpsc channel 发送，无需直接访问 registry
- 无并发访问，plain HashMap 足够

## 影响

- `execute_stream` 签名简化为 3 参数：`(state, sink, trace_id)`
- 无 `pending_decisions` 参数传递
- `DecisionRegistry` 是 executor 私有状态，不暴露给 Node

## 相关决策

- ADR-000: Architecture Review Gate（架构评审门）
