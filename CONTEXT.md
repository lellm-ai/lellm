# LeLLM 领域术语表

> 持续更新，作为架构讨论和决策的共享词汇。

## 核心概念

| 术语 | 定义 |
|------|------|
| **Protocol** | 协议层（lellm-core），定义 Message、ToolCall、Request/Response 等标准化数据格式，零运行时依赖 |
| **LlmProvider** | LLM 调用抽象，`call()` / `stream()` 两个方法；通过 CodecProvider + ChatCodec 实现三权分立（HTTP 传输 / 协议编解码 / 配置） |
| **FlowNode** | 图编排引擎中的节点抽象，`execute(&self, ctx: &mut FlowContext) -> NodeOutput` |
| **Graph** | 有向图编排引擎，支持有环图、并行执行、Barrier（人类介入）、Checkpoint |
| **State** | 图执行的状态容器，legacy 为 `HashMap<String, Value>` 包装 |
| **StateDelta** | 节点输出的增量变更，支持 Reducer 合并，不直接修改 State |
| **WorkflowState** | 强类型状态框架（v0.4+ 方向），trait 定义 Effect + apply 语义 |
| **Agent** | 智能体运行时，封装 ToolUseLoop（ReAct 循环） |
| **ToolUseLoop** | ReAct 循环的核心实现，LLM 调用 → 工具执行 → 再调用的迭代 |
| **ReAct Graph** | 用 Graph 原语（LLMNode, ToolNode, BudgetCondition 等）表达的 ReAct 循环，v0.4 新路径 |
| **ToolCatalog** | 工具注册表，管理工具的 ParallelSafety 分类和批量执行 |
| **ParallelSafety** | 工具并行安全模型：Safe / CategoryExclusive / Exclusive |
| **Barrier** | 人类介入节点，必须配置超时，level-triggered 决策机制 |
| **Checkpoint** | 执行快照，默认每步触发，支持增量恢复 |
| **AgentFlowNode** | Agent 到 FlowNode 的适配器，让 Agent 可作为图节点使用 |

## 架构红线

1. **graph ↛ agent** — Graph 是通用引擎，Agent 是上层消费者
2. **provider ↛ graph** — Provider 只负责 LLM 调用
3. **mcp ↛ agent** — MCP 是独立协议域

## StreamChunk 流式协议 — Execution View

Graph 层定义统一的流式事件协议。StreamChunk 携带**展示内容**（Execution View），不是 Message。
State 保存完整 `Message::ToolResult`。两者永不互相引用。

```rust
pub enum StreamChunk {
    TextDelta(String),
    ThinkingDelta(String),
    ToolLifecycle { phase: ToolPhase, call_id: String, tool_name: String },
    ToolOutput { call_id: String, tool_name: String, content: String, is_error: bool },
}
```

其中 `ToolPhase`:
```rust
pub enum ToolPhase {
    Queued,
    Started,
    Finished,
}
```

**State Plane** — `Message::ToolResult`（完整，含 content_blocks, metadata, raw_response）
**Data Plane** — `ToolOutput`（展示用，content 为 String，前端直接展示）

**Start 保证顺序** — 严格按照 ToolCall 顺序发射（A, B, C）。
**End 允许乱序** — 并发执行完成后按实际顺序发射（B, A, C），通过 call_id 关联。

这与 LangGraph (`on_tool_start`/`on_tool_end`)、OpenAI Agents SDK、Claude Desktop 一致。

## StreamSink 抽象

Graph 层定义 `StreamSink` trait，只声明 `emit(chunk)` — Producer Push 模型。
Graph 不知道 channel、WebSocket、Logger。所有消费端实现都在 Agent/Provider 层。

```rust
pub trait StreamSink: Send + Sync {
    fn emit(&self, chunk: StreamChunk);
}
```

Agent 层提供 `ChannelSink`（包装 `mpsc::Sender<AgentEvent>` + `CancellationToken`）。

`Graph::run_inline_stream(state, sink)` 接受 sink，不返回 Receiver。

## Step Boundary Commit

Token 流式过程只走 Stream（emit），不写 State。
LLMNode 内部累积 chunks → 流结束 → 一次性 `emit_effect(AppendMessage(...))`。
State 始终满足 step 边界提交，与 LangGraph / Pregel 一致。

## 取消策略

`CancellationToken` 嵌入 Sink。消费者 drop Receiver → 触发 cancel → 所有 Node 检查 cancel → 返回 `GraphError::Cancelled`。
比 LangGraph 的 GeneratorExit 更主动、更立即。

## 版本状态

- **v0.4** — 当前。ReAct Graph 已实现（含流式路径），Typed State 框架已建立
- **ReAct Graph** — `AgentFlowNode` 支持 `use_react_graph` 开关，内部构建 `Graph<AgentState>` 驱动 LLM→Tool→LLM 循环
- **流式** — LLMNode 内部使用 `stream()` + `StreamExt` 收集事件，同时 emit StreamChunk 到 ctx
- **待完善** — ReAct Graph 模式的 `run_inline` 不产生 `AgentEvent` 流（hook snapshot 传空数组），待后续补充
