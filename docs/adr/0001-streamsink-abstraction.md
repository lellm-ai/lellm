# ADR-0001: StreamSink 抽象 — Producer Push 模型

**日期：** 2026-06-25
**状态：** Accepted
**类型：** Architecture

## 上下文

LeLLM Graph 层需要支持流式执行（如 LLM token 流式输出）。
初始设计让 `Graph::execute_stream()` 返回 `mpsc::Receiver<T>`，
将 Tokio channel 实现泄漏到 Graph 层。

这违反了两条核心原则：
1. Graph 是执行引擎，不是 Tokio Channel Framework
2. Graph 层应该只依赖抽象，不依赖具体传输机制

## 决策

### StreamSink Trait

```rust
pub trait StreamSink: Send + Sync {
    fn emit(&self, chunk: StreamChunk);
}
```

- 同步 `emit` — Node 永远不阻塞
- Producer Push 模型 — 生产者推送，不感知消费者
- Graph 只知道 trait，不知道 channel、WebSocket、Logger

### BufferedSink + Forward Task

```
LLMNode
   ↓ emit() — O(1), 固定成本
BufferedSink (SegQueue / UnboundedSender)
   ↓
Forward Task (spawn)
   ↓
mpsc::Sender<StreamChunk>
   ↓
Consumer
```

- Node → BufferedSink：同步，O(1)
- BufferedSink → Forward Task：异步消费
- Forward Task → Consumer：backpressure 在此层处理

### 取消 = 消费者离开（不是背压）

```
consumer gone
    ↓
receiver dropped
    ↓
forward task exit
    ↓
CancellationToken.cancel()
    ↓
所有 Node 检查 cancel → 停止
```

- 取消 ≠ 背压
- 取消 = 消费者离开
- CancellationToken 嵌入 NodeContext，所有 Node 定期检查

### Step Boundary Commit

Token 流式只走 Stream（emit），不写 State。
LLMNode 内部累积 chunks → 流结束 → 一次性 `emit_effect(AppendMessage(...))`。
State 始终满足 step 边界提交。

### StreamChunk — Execution View

StreamChunk 携带**展示内容**（Execution View），不是 Message。
State 保存完整 `Message::ToolResult`。两者永不互相引用。

```rust
pub enum StreamChunk {
    TextDelta(String),
    ThinkingDelta(String),
    ToolLifecycle { phase: ToolPhase, call_id: String, tool_name: String },
    ToolOutput { call_id: String, tool_name: String, content: String, is_error: bool },
}

pub enum ToolPhase {
    Queued,
    Started,
    Finished,
}
```

**State Plane** — `Message::ToolResult`（完整，含 content_blocks, metadata, raw_response）
**Data Plane** — `ToolOutput`（展示用，content 为 String，前端直接展示）

### Tool 并发 emit 协议

- **Start 保证顺序** — 严格按照 ToolCall 顺序发射（A, B, C）
- **End 允许乱序** — 并发执行完成后按实际顺序发射（B, A, C），通过 call_id 关联
- 每个工具完成后立即 emit `ToolLifecycle::Finished` + `ToolOutput`

### Graph API 统一

`Graph::run_inline_stream(state, sink)` 统一流式与阻塞。
sink=None 等价于阻塞执行。删除旧的 `run_inline`。

### Agent API 统一

`ToolUseLoop::execute_stream()` 统一流式与阻塞。
提供 `ChannelSink` 包装 `mpsc::Sender<AgentEvent>` + `CancellationToken`。
删除旧的 `execute()`。

## 后果

**正面：**
- Graph 层完全解耦传输机制
- Node 执行成本固定，不受消费者速度影响
- 取消传播立即、主动
- 测试可以通过 mock `StreamSink` 验证行为
- 与 LangGraph / Tokio / Actor Model 一致
- StreamChunk 成为一等协议，Graph/Agent/MCP 共享

**负面：**
- BufferedSink 在极端情况下可能占用内存（消费者极慢）
- 需要 Forward Task 生命周期管理
- 比直接 channel 多一层间接
- `execute()` 删除是破坏性变更

## 影响范围

| 模块 | 变更 |
|------|------|
| `lellm-graph/src/stream_chunk.rs` | 重构 StreamChunk enum，加入 ToolLifecycle/ToolOutput |
| `lellm-graph/src/stream_emitter.rs` | 改为 `StreamSink` trait + `BufferedSink` |
| `lellm-graph/src/node_context.rs` | 添加 `cancel: CancellationToken`，stream 改为 `&dyn StreamSink` |
| `lellm-graph/src/graph.rs` | 统一 `run_inline_stream(state, sink)`，删除旧的 `run_inline` |
| `lellm-agent/src/runtime/runtime.rs` | `execute_stream()` 重写，提供 `ChannelSink`，删除 `execute()` |
| `lellm-agent/src/runtime/react.rs` | LLMNode 增加流式模式；ToolNode 重构并发 emit |
| `lellm-agent/src/runtime/state.rs` | **删除** — ~380 行 HashMap helpers |
| `lellm-agent/src/runtime/iteration.rs` | **重构** — `emit_and_execute_tools_with` 适配 StreamSink |

## 参考

- LangGraph `StreamProtocol` — 回调式 Push 模型
- LangGraph `on_tool_start`/`on_tool_end` — 工具生命周期事件
- Tokio `CancellationToken` — 协作式取消
- Pregel Step Boundary — 批量提交语义
- OpenAI Agents SDK `tool_call_started`/`tool_call_finished`
- Claude Desktop `tool_use`/`tool_result`
