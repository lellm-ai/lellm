# ADR-0002: 统一执行路径 + LlmInvoker 分层

**日期：** 2026-06-28
**状态：** Accepted
**类型：** Architecture

## 上下文

`ToolUseLoop` 存在两条执行路径：
- `execute()` — 走 `Graph<AgentState>` + react graph（~60 行）
- `execute_stream()` — 手写 while 循环（~250 行），绕过 Graph

每次修改 Agent 循环逻辑（BudgetCondition、Compactor、Retry、ToolNode 事件）必须在两个地方应用，违反了 locality 和 leverage 原则。

同时，`lellm-graph/src/hook.rs`（229 行）是死代码——executor 从未调用，且与 agent 层 `AgentHook` 同名冲突，泄漏了 agent 概念到 graph 层。

## 决策

### 1. 删除 Graph 层 hook.rs

- 删除 `lellm-graph/src/hook.rs`（229 行死代码）
- 消除 `AgentHook` 同名 trait 造成的命名冲突
- 修复 graph → agent 概念泄漏（违反红线 1）

### 2. StreamSink 是唯一的消费抽象

```rust
pub trait StreamSink: Send + Sync {
    fn emit(&self, chunk: StreamChunk);
}
```

- Graph 只负责生产（emit），不参与消费
- 没有 `StreamAdapter` trait——Adapter 是协议转换（Provider 层），消费端叫 Sink
- 没有 `on_finish()`——Rust 的 `Drop` + channel close 就是 finish
- 暂不演进为 `RuntimeSink`，等真正需要控制面事件时再做

### 3. 统一执行路径

```
AgentRuntime
    │
    ▼
Graph::run_inline_stream(state, sink)
    │
    ▼
StreamSink (如 AgentEventSink)
    │
    ▼
Agent API
```

`execute_stream()` 不再包含任何 ReAct/Tool/Budget/Fallback 业务逻辑，只负责：
1. 创建 channel
2. 创建 AgentEventSink
3. 调用 `graph.run_inline_stream(state, sink)`
4. 返回 `Receiver<AgentEvent>`

### 4. LlmInvoker 分层

```
AgentRuntime
    │
    ▼
ReAct Graph <── 只负责调度节点
    │
    ▼
LLMNode <────── 只负责 State ↔ Request ↔ Effect
    │
    ▼
LlmInvoker <─── 只负责获得一次成功的调用
    │  (retry, fallback, circuit breaker,
    │   stream state machine, metrics, tracing)
    │
    ▼
LlmProvider <── 只负责 protocol adapter (stateless)
    │
    ▼
HTTP Client
```

**LlmInvoker** — struct（不是 trait），内部持有 `InvocationPlan`：

```rust
pub struct LlmInvoker {
    plan: InvocationPlan,
    // ...
}

pub struct InvocationPlan {
    attempts: Vec<InvocationAttempt>,
}

pub struct InvocationAttempt {
    provider: Arc<dyn LlmProvider>,
    retry_policy: RetryPolicy,
    timeout: Duration,
    temperature: Option<f32>,
    reasoning: Option<ReasoningEffort>,
}
```

**Stream State Machine** 决定 retry 边界：

```rust
enum StreamState {
    NotStarted,       // retry OK
    HeadersReceived,  // retry OK
    FirstChunkSent,   // abort (token 不可撤销)
    Finished,         // impossible
}
```

**LlmProvider** 保持 stateless protocol adapter。不承载 retry、fallback、metrics。

**不做 ToolInvoker** — 工具不需要 Invoker 层。`ToolExecutor + RetryPolicy` 已够用。工具没有 "fallback 到另一个工具" 的需求。

## 实施顺序

1. 创建 `AgentEventSink` — 实现 `StreamSink`，内部做 `StreamChunk → AgentEvent` 转换
2. 创建 `LlmInvoker` struct — 包装 `InvocationPlan`，实现 stream state machine + retry + fallback
3. 改造 `LLMNode` — 接收 `Arc<LlmInvoker>` 替代 `ResolvedModel + ToolExecutor + deps`
4. 改造 `execute_stream()` — 删掉手写 while 循环，变成薄壳
5. 删除 `iteration.rs` 中流式专用代码
6. 清理 `runtime.rs` 中被 typed state 替代的 State 辅助函数

## 后果

**正面：**
- Agent 循环逻辑集中在 react.rs 一个 module（locality）
- 一处修 bug，阻塞+流式两条路径都受益（leverage）
- Graph 层不感知 Agent 概念（保持 seam 干净）
- LLMNode 变薄，复杂策略沉到 LlmInvoker（职责清晰）
- Provider 保持 stateless，可被任意 Invoker 策略复用
- 删除 ~500 行重复/死代码

**负面：**
- AgentEventSink 需要完整覆盖 StreamChunk → AgentEvent 的转换逻辑
- LlmInvoker 是新组件，需要充分的集成测试
- `execute()` 的现有测试需要适配新接口

## 影响范围

| 模块 | 变更 |
|------|------|
| `lellm-graph/src/hook.rs` | **删除** |
| `lellm-graph/src/lib.rs` | 移除 hook 相关 pub mod/use |
| `lellm-agent/src/runtime/runtime.rs` | 重写 execute_stream() 为薄壳 |
| `lellm-agent/src/runtime/react.rs` | LLMNode 接收 LlmInvoker |
| `lellm-agent/src/runtime/invoker.rs` | **新增** — LlmInvoker + InvocationPlan |
| `lellm-agent/src/runtime/event_bridge.rs` | **新增** — AgentEventSink |
| `lellm-agent/src/runtime/iteration.rs` | 删除流式专用代码 |