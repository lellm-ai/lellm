# LeLLM v0.1 设计文档

> 日期: 2026-06-03 | 配合 [BLUEPRINT.md](./BLUEPRINT.md) 使用
> BLUEPRINT.md 记录产品蓝图和完整 API 契约，本文档记录关键设计决策的 **为什么**。

## 1. MaxIterationsReached — Ok 还是 Err？

**决定：** `Ok(ToolUseResult { stop_reason: StopReason::MaxIterationsReached, ... })`

**理由：**
- `MaxIterationsReached` 是 Agent 层的控制流决策，不是 Provider 的错误
- 返回 `Ok` 让调用方统一处理 `ToolUseResult`，通过 `stop_reason` 区分结果类型
- 如果返回 `Err`，调用方需要同时处理 `Ok` 和 `Err` 两种路径才能拿到 `messages` 和 `iterations`

**execute() 语义：**
- `Ok(ToolUseResult)` — Agent 层完成（含 Complete、MaxIterationsReached）
- `Err(LlmError)` — Provider 调用失败

## 2. AgentEvent 终态契约

**决定：** `Receiver<AgentEvent>`（不含 `Result`），`LoopEnd`/`LoopError` 为终态变体。

**终态契约：**
1. 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
2. 业务错误：`LoopError` 恰好一次，然后 channel 关闭
3. 终态事件后不再发送任何事件
4. 如果 channel 关闭前未收到 `LoopEnd` 或 `LoopError`，视为 Agent Runtime 异常中断（panic / abort / OOM / runtime shutdown 等）
5. 不增加 `StreamClosed` 变体——channel 关闭本身就是信号

**为什么不用 `Receiver<Result<AgentEvent, LlmError>>`：**
- `LoopEnd` 需要传递 `ToolUseResult`（含 `stop_reason`、`iterations`、`tool_calls_executed`）
- 如果用 `Result` 包裹，无法区分「loop 正常结束」和「Provider 错误」
- `LoopError` 携带 `iterations` 和 `messages` 上下文，比单纯 `LlmError` 更丰富

**未来方向：** 可在 Runtime 层引入 `tokio-util` 的 `CancellationToken` 实现取消机制，与终态协议独立。

## 3. ToolUseLoop 不知道 Router / Registry

**决定：** `ToolUseLoop` 接收 `ResolvedModel`（已绑定的 provider + model），不依赖 `ModelRouter` 或 `ProviderRegistry`。

**理由：**
- 关注点分离：路由决策在外部，loop 只负责执行
- 可测试性：测试时直接传入 `MockProvider`，无需构造 Router
- 灵活性：调用方可实现自定义路由策略（如基于成本的动态选择）

**使用模式：**
```rust
let route = router.resolve(TaskLevel::Flash)?;
let resolved = registry.resolve(route)?;
ToolUseLoop::new(resolved, executor).execute(messages).await
```

## 4. ResolvedModel 放在 provider crate

**决定：** `ResolvedModel` 定义在 `lellm-provider`，`lellm-agent` 通过 `pub use lellm_provider::ResolvedModel` 再导出。

**理由：**
- `ResolvedModel` 绑定 `Arc<dyn LlmProvider>`，自然属于 provider 层
- `ProviderRegistry::resolve()` 返回 `ResolvedModel`，同 crate 内更自然
- agent 层只是消费者，不应定义这个类型

## 5. ProviderAdapter 的 build_request 签名

**决定：** `build_request(&self, req: &ChatRequest, config: &ProviderConfig, stream: bool)`

**理由：**
- Adapter 需要 `config` 来构建 URL（base_url + /v1/chat/completions）和 headers（api_key）
- `stream` 标志让 Adapter 知道是否需要添加 `Accept: text/event-stream` 等流式 header
- 比让 Adapter 持有 config 更灵活（同一 adapter 可适配不同配置）

## 6. GenericProvider 已实现 LlmProvider

**决定：** `GenericProvider<A: ProviderAdapter + Clone>` 自动 `impl LlmProvider`。

**关键实现细节：**
- SSE 行缓冲：`bytes_stream()` 可能截断单条 SSE 消息，按 `\n` 分割后交给 adapter 解析
- `ToolCallAccumulator` 在 GenericProvider 内部组装增量 delta
- `ProviderAdapter` 是 `pub(crate)`，外部只能通过 `LlmProvider` trait 使用

## 7. FallbackAction::SwitchProvider 用 String 而非 RouteEntry

**决定：** `SwitchProvider(String)` — 仅保存 provider_id，不绑定具体路由。

**理由：**
- v0.1 仅实现 Retry + Abort，`SwitchProvider` 保留变体但不实现
- 用 `String` 保持简单，v0.2 实现时再改为 `RouteEntry`

## 8. 文件命名：loop_.rs → runtime.rs

**决定：** Agent 运行时主文件命名为 `runtime.rs`。

**理由：**
- `loop_` 的尾缀下划线是 Python 风格，Rust 中不自然
- 文件内容已远超「loop」概念，包含 `ToolUseLoop`、`execute_stream`、`ToolUseResult`、`ToolCallResult`
- `runtime.rs` 更准确地描述了文件的职责

## 9. AgentEvent::ToolEnd.result 用 ToolCallResult

**决定：** `ToolEnd { tool_call_id, result: ToolCallResult }`，而非 `result: String`。

**理由：**
- `ToolCallResult::Ok` / `ToolCallResult::Err` 明确区分工具执行成功与失败
- 消费者可根据结果类型做不同处理（如错误时触发降级）
- 比单纯 `String` 携带更多语义信息
