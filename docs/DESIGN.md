# LeLLM v0.1 设计文档

> 日期: 2026-06-03 | 配合 [BLUEPRINT.md](./BLUEPRINT.md) 使用
> BLUEPRINT.md 记录产品蓝图和完整 API 契约，本文档记录关键设计决策的 **为什么**。

## 0. `lellm` 门面 Crate

**决定：** 保留 `lellm` 作为统一入口，feature-gated re-export 所有子 crate。

**理由：**
- 用户体验：`cargo add lellm` 一键引入，无需记忆子 crate 名称
- 精准依赖：高级用户可直接 `cargo add lellm-agent` 避免拉入未使用的 crate
- 命名空间访问：`lellm::core::Message`、`lellm::provider::*`、`lellm::agent::*`
- Rust 生态先例：`tokio`、`actix-web` 等均采用门面模式

**Feature Gate：**
- `provider`（默认）— re-export core + provider
- `agent` — re-export core + agent（传递依赖 provider）
- `macros` — re-export macros
- `full` — 以上全部

## 1. MaxIterationsReached — Ok 还是 Err？

**决定：** `Ok(ToolUseResult { stop_reason: StopReason::MaxIterationsReached, ... })`

**理由：**
- `MaxIterationsReached` 是 Agent 层的控制流决策，不是 Provider 的错误
- 返回 `Ok` 让调用方统一处理 `ToolUseResult`，通过 `stop_reason` 区分结果类型
- 如果返回 `Err`，调用方需要同时处理 `Ok` 和 `Err` 两种路径才能拿到 `messages` 和 `iterations`
- 类似 `http::Response` — 200/404/500 都是 `Ok(Response)`，状态在 body 里

**execute() 语义：**
- `Ok(ToolUseResult)` — Agent 层完成（含 Complete、MaxIterationsReached）
- `Err(LlmError)` — Provider 调用失败

**安全网：** `ToolUseResult` 必须实现 `is_success() -> bool`，仅 `StopReason::Complete` 返回 `true`。调用方不应盲目 `?` 解包后直接使用，应先检查 `stop_reason` 或调用 `is_success()`。

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
- `LoopError` 只需携带 `error` + `iterations`，无需 `messages`

**`LoopError` 不携带 `messages` 的设计原则：**
- **成功路径返回完整结果** — `LoopEnd.result: ToolUseResult` 包含 `response`、`messages`、`iterations`、`stop_reason`
- **错误路径返回错误原因** — `LoopError` 只告诉消费者「为什么失败」和「跑了多少轮」
- **历史状态由 stream 消费者自行维护** — 需要诊断的消费者自己累积 `Vec<Message>`
- **防止 partial_* 字段蔓延** — 一旦开了 `partial_messages`，必然引来 `partial_response`、`partial_tool_calls`、`partial_usage`，最终退化为复制整个运行时状态

**未来方向：** 可在 Runtime 层引入 `tokio-util` 的 `CancellationToken` 实现取消机制，与终态协议独立。

## 3. ToolUseLoop 不知道 Router / Registry — model 单向流动

**决定：** `ToolUseLoop` 接收 `ResolvedModel`（已绑定的 provider + model），不依赖 `ModelRouter` 或 `ProviderRegistry`。

**理由：**
- 关注点分离：路由决策在外部，loop 只负责执行
- 可测试性：测试时直接传入 `MockProvider`，无需构造 Router
- 灵活性：调用方可实现自定义路由策略（如基于成本的动态选择）

**model 单向流动：**
- `ResolvedModel.model` 是**唯一来源** — 路由层决定用什么模型
- `ToolUseLoop` 内部注入 `ChatRequest.model = self.model.model.clone()`
- 用户调用 `loop.execute(messages)`，**不需要碰 model**
- 没有覆盖机制，没有二义性

**使用模式：**
```rust
let route = router.resolve(TaskLevel::Flash)?;
let resolved = registry.resolve(route)?;
ToolUseLoop::new(resolved, executor).execute(messages).await
// ↑ 只需传 messages，model 由 ResolvedModel 注入
```

## 4. ResolvedModel 放在 provider crate

**决定：** `ResolvedModel` 定义在 `lellm-provider`，`lellm-agent` 通过 `pub use lellm_provider::ResolvedModel` 再导出。

**理由：**
- `ResolvedModel` 绑定 `Arc<dyn LlmProvider>`，自然属于 provider 层
- `ProviderRegistry::resolve()` 返回 `ResolvedModel`，同 crate 内更自然
- agent 层只是消费者，不应定义这个类型

## 5. ProviderAdapter 不知 ProviderConfig — 方案 D：ProviderRequest 中间层

**决定：** `ProviderAdapter` 完全不知道 `ProviderConfig` 和 `reqwest`。通过 `ProviderRequest` 中间层解耦。

```rust
// ─── Adapter 产生的中间表示 ───
pub struct ProviderRequest {
    pub path: &'static str,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
}

pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn build_request(
        &self,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<ProviderRequest, LlmError>;
    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;
    fn parse_stream_chunk(&self, event: &SseEvent) -> Result<StreamParseResult, LlmError>;
}
```

**`GenericProvider` 持有 config + client，统一组装 HTTP 请求：**
```rust
pub struct GenericProvider<A: ProviderAdapter> {
    adapter: A,
    config: ProviderConfig,
    client: reqwest::Client,
}

// GenericProvider 侧组装逻辑：
let pr = adapter.build_request(req, stream)?;
let url = format!("{}{}", self.config.base_url, pr.path);
let mut builder = self.client.post(&url)
    .timeout(self.config.timeout);
// 注入认证 header (Bearer / Header / None)
if let Some((k, v)) = self.config.auth.get_header() {
    builder = builder.header(&k, &v);
}
for (k, v) in pr.headers {
    builder = builder.header(k, v);
}
builder = builder.json(&pr.body);
```

**职责切分：**

| 职责 | Adapter | GenericProvider |
|------|---------|-----------------|
| Endpoint 路径 | ✅ (`/v1/chat/completions`) | ❌ |
| JSON 请求体格式 | ✅ | ❌ |
| 协议特定 Header | ✅ (`anthropic-version`) | ❌ |
| HTTP Client | ❌ | ✅ |
| base_url | ❌ | ✅ |
| api_key | ❌ | ✅ |
| timeout | ❌ | ✅ |
| retry / proxy / tls | ❌ | ✅ |

**`ProviderConfig` 精简为连接配置（不含 model）：**
```rust
pub struct ProviderConfig {
    base_url: Url,
    auth: AuthConfig,       // Bearer / Header / None
    timeout: Duration,
}
```

**理由：**
- **Adapter 可独立测试** — 不依赖 `reqwest`，直接断言 `ProviderRequest` 字段
- **`reqwest` 不泄漏到 Adapter API** — Adapter 只依赖 `http` crate 的 `HeaderMap`
- **成熟 SDK 分层模式** — AWS SDK、Stripe SDK、Kubernetes client 都采用此模式
- **`ChatRequest` 自包含 model** — `model` 不在 `ProviderConfig` 中，每次请求显式指定
- **`parse_stream_chunk` 接收 `&SseEvent`** — SSE 行缓冲由 `GenericProvider` 统一处理，Adapter 只解析 JSON payload

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
