# LeLLM v0.1 设计文档

> 日期: 2026-06-03 | 配合 [BLUEPRINT.md](./BLUEPRINT.md) 使用
> BLUEPRINT.md 记录产品蓝图和核心 API 契约，本文档记录关键设计决策的 **为什么** 与 **如何实现**。

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
- 类似 `http::Response` — 200/404/500 都是 `Ok(Response)`，状态在 body 里

**execute() 语义：**
- `Ok(ToolUseResult)` — Agent 层完成（含 Complete、MaxIterationsReached）
- `Err(LlmError)` — Provider 调用失败

**安全网：** `ToolUseResult.is_success()` 仅 `StopReason::Complete` 返回 `true`。

## 2. AgentEvent 终态契约

**决定：** `Receiver<AgentEvent>`（不含 `Result`），`LoopEnd`/`LoopError` 为终态变体。

**终态契约：**
1. 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
2. 业务错误：`LoopError` 恰好一次，然后 channel 关闭
3. 终态事件后不再发送任何事件
4. 如果 channel 关闭前未收到 `LoopEnd` 或 `LoopError`，视为 Agent Runtime 异常中断
5. 不增加 `StreamClosed` 变体——channel 关闭本身就是信号

**`LoopError` 不携带 `messages`：**
- 成功路径返回完整结果 — `LoopEnd.result` 含 response、messages、iterations、stop_reason
- 错误路径只返回原因 — `LoopError` 只含 error + iterations
- 防止 `partial_*` 字段蔓延

**消费者标准写法：**
```rust
let mut saw_terminal = false;
while let Some(event) = rx.recv().await {
    if matches!(event, AgentEvent::LoopEnd { .. } | AgentEvent::LoopError { .. }) {
        saw_terminal = true;
    }
}
if !saw_terminal {
    error!("agent runtime crashed");
}
```

## 3. ToolUseLoop 不知道 Router / Registry — model 单向流动

**决定：** `ToolUseLoop` 接收 `ResolvedModel`（已绑定的 provider + model），不依赖 `ModelRouter` 或 `ProviderRegistry`。

**model 单向流动：**
```
ResolvedModel.model  ← 路由层唯一来源
       ↓ (ToolUseLoop 注入)
ChatRequest.model    ← 实际发送给 Provider 的模型
       ↓
CodecProvider        ← 只读取 ChatRequest.model
```

**使用模式：**
```rust
let route = router.resolve(TaskLevel::Flash)?;
let resolved = registry.resolve(route)?;
ToolUseLoop::new(resolved, executor).execute(messages).await
// ↑ 只需传 messages，model 由 ResolvedModel 注入
```

## 4. ResolvedModel 放在 provider crate

`ResolvedModel` 绑定 `Arc<dyn LlmProvider>`，自然属于 provider 层。agent 层通过 `pub use` 再导出。

## 5. ProviderCodec — 三权分立的协议编解码 SPI

> **v2026-06-10 重构：** `ProviderAdapter` 拆分为三个独立 trait + `ProviderExtension` 超级 trait。

**背景：** 原 `ProviderAdapter` trait 混杂了三类职责——协议编解码、能力声明、连接元数据——导致 trait 臃肿、难以独立演进。

**决定：** 拆分为三个正交 trait，通过 `ProviderExtension` 超级 trait 统一消费：

### 5.1 ChatCodec — 协议编解码（物理层互转）

```rust
pub trait ChatCodec: Send + Sync {
    fn encode(&self, req: &ChatRequest, stream: bool) -> Result<CodecRequest, LlmError>;
    fn decode(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;
    fn decode_sse(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;
}
```

**职责：** 纯粹的物理层互转。Codec **不知道** `CodecProvider`、`reqwest`、HTTP。

**中间表示：**
```rust
pub struct CodecRequest {
    pub path: Cow<'static, str>,    // "/v1/chat/completions"
    pub headers: HeaderMap,          // 协议特定 Header
    pub body: Bytes,                 // 序列化后的 JSON
}

pub enum StreamChunk {
    TextDelta(String),
    ThinkingDelta { thinking: String, redacted: Option<String> },
    ToolCallDelta(ToolCallDelta),
    Usage(TokenUsage),
    InputTokens(u32),
    OutputTokens(u32),
    Done,
}
```

### 5.2 ModelCapabilities — 能力声明（逻辑校验层）

```rust
pub trait ModelCapabilities: Send + Sync {
    fn capabilities_for(&self, model: &str) -> Capabilities;
}

pub struct Capabilities {
    pub supports_image_input: bool,
    pub supports_reasoning: bool,
    pub supports_tool_call: bool,
}
```

**设计原则：**
- 模型感知——不同模型支持不同能力，Codec 通过 `capabilities_for(model)` 精确声明
- **移除 `heuristic_guess()`**——不再基于模型名猜测能力，Codec 必须精确实现
- v0.1 最小集（image, reasoning, tool_call），v0.2 按需扩展

### 5.3 ProviderMeta — 连接元数据（控制层）

```rust
pub trait ProviderMeta: Send + Sync {
    fn provider_id(&self) -> &str;
    fn default_base_url(&self) -> &'static str;
    fn auth_style(&self) -> AuthStyle;
    fn default_headers(&self) -> HeaderMap { HeaderMap::new() }
    fn api_key_env(&self) -> Cow<'static, str>;  // 默认 {PROVIDER_ID}_API_KEY
}
```

`default_headers()` 允许 Codec 声明协议必需的默认 Headers（如 Anthropic 的 `anthropic-version`）。
与 Builder 传入的 extra_headers 以及 CodecRequest 的 headers 三层合并：
**codec defaults → builder extra_headers → request headers**（后者覆盖前者）。

### ProviderExtension — 生态扩展统一入口

```rust
pub trait ProviderExtension: ChatCodec + ModelCapabilities + ProviderMeta {}
// 毯式实现：任何同时实现三个 trait 的类型，自动成为 ProviderExtension
```

开发者实现新 Provider 时，只需实现 `ProviderExtension`（或其三个子 trait），框架内部按需消费。

### CodecProvider — 持有 Codec + 连接配置

> 原 `GenericProvider` 已重命名为 `CodecProvider`。

```rust
pub struct CodecProvider<C: ProviderExtension> {
    codec: Arc<C>,        // Arc 共享，无需 Clone bound
    config: ProviderConfig,
    client: reqwest::Client,
    extra_headers: HeaderMap,  // Builder 传入的额外 Headers
}
```

**职责切分：**

| 职责 | ChatCodec | stream/ 模块 | CodecProvider |
|------|-----------|-------------|---------------|
| Endpoint 路径 | ✅ | ❌ | ❌ |
| JSON 请求体格式 | ✅ | ❌ | ❌ |
| 协议特定 Header | ✅ | ❌ | ❌ |
| SseFrame → StreamChunk 解析 | ✅ | ❌ | ❌ |
| SseParser (行缓冲 + SseFrame) | ❌ | ✅ | ❌ |
| ToolCallAccumulator | ❌ | ✅ | ❌ |
| process_stream (管道编排) | ❌ | ✅ | ❌ |
| EventSink / StreamEvent | ❌ | ✅ (trait) | ❌ |
| HTTP Client | ❌ | ❌ | ✅ |
| base_url / api_key / timeout | ❌ | ❌ | ✅ |
| ChannelSink (桥接) | ❌ | ❌ | ✅ |

### ProviderBuilder — 链式构建

> **v2026-06-11 新增：** `CodecProvider` 通过 `ProviderBuilder` 构建，支持自定义 Headers。

**背景：** OpenRouter 等聚合网关需要自定义 Headers（`HTTP-Referer`、`X-Title`），但不同网关的 Header 需求不同。写死在 Codec 中不够灵活，纯 `ProviderConfig` 又缺少 Header 支持。

**决定：** 引入 `ProviderBuilder<C>`，链式 API 构建 `CodecProvider`：

```rust
let provider = CodecProvider::builder(OpenAICompatCodec::openai())
    .base_url("https://openrouter.ai/api/v1")
    .api_key("sk-or-...")
    .header("HTTP-Referer", "https://mysite.com")
    .header("X-Title", "My App")
    .build()?;
```

**Header 合并优先级：** codec defaults → builder extra_headers → request headers（后者覆盖前者）。

**OpenRouter 便捷函数：**
```rust
// 从 OPENROUTER_API_KEY 环境变量加载
let provider = openrouter(OpenAICompatCodec::openai())?;

// 换协议只需换 Codec
let anthropic_via_openrouter = openrouter(AnthropicCodec)?;
```

### ProviderConfig — 连接配置

```rust
pub struct ProviderConfig {
    pub base_url: url::Url,
    pub auth: AuthConfig,
    pub connect_timeout: std::time::Duration,
    pub timeout: std::time::Duration,
    pub idle_timeout: std::time::Duration,
}
```

**环境变量自动加载：**
```rust
// 便捷方法 — 一行搞定
let provider = CodecProvider::from_env(OpenAICompatCodec::openai())?;

// 自定义超时
let codec = OpenAICompatCodec::openai();
let provider = CodecProvider::new(
    codec.clone(),
    ProviderConfig::from_codec(&codec)?
        .with_timeout(Duration::from_secs(60)),
);
```

环境变量前缀 = `provider_id().to_ascii_uppercase()`。

### AuthConfig — apply() 替代 get_header()

**问题：** `get_header()` 将 `SecretString` 展开为明文 `String`，意外传播认证细节。

**决定：** 删除 `get_header()`，改为 `apply(builder) -> builder`：

```rust
impl AuthConfig {
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder;
}
```

**好处：**
1. **Secret 生命周期最短** — `expose_secret()` 在 `header()` 内部直接消费
2. **不暴露认证细节** — `CodecProvider` 不需要知道 `Authorization`、`Bearer` 等协议细节
3. **未来可扩展** — 支持 `AuthConfig::OAuth` 时只需加一个变体，API 不变

### Trait 可见性

> **v0.1 决策：** 核心 trait 全部公开（`pub`），外部开发者可实现自定义 Provider。

`ChatCodec`、`ModelCapabilities`、`ProviderMeta`、`ProviderExtension`、`CodecProvider` 均为 `pub`。
`CodecRequest`、`StreamChunk`、`StreamParseResult` 随之公开。
`stream/` 模块保持 `pub(crate)`（传输层细节不对外暴露）。

### SSE 解析 — SseFrame 中间表示

**决定：** `SseParser` 负责 SSE 协议解析（行缓冲、`data:` 提取、空行检测），构建 `SseFrame` 交给 ChatCodec 解析 JSON payload。

```rust
pub(crate) struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}
```

ChatCodec 的 `decode_sse()` 方法接收 `SseFrame`，返回 `StreamParseResult<Vec<StreamChunk>>`。

**理由：**
- Codec 完全不知道 SSE 协议细节，只关心 `event` 类型和 `data` 内容
- `event` 字段对 Anthropic 等 provider 有用（区分 `message_start` / `content_block_delta` / `message_stop`）
- OpenAI 的 `[DONE]` 直接出现在 `data` 字段中，Codec 自行判断
- SSE 行缓冲只在 `SseParser` 一处实现，可独立测试

### 流式处理 — 传输层解耦（EventSink + StreamEvent）

**问题：** `process_stream()` 直接接收 `reqwest::Response`，耦合 HTTP 客户端。测试需要 mockito/wiremock。

**决定：** `process_stream()` 只认识 `Stream<Item = Result<Bytes, LlmError>>` 和 `EventSink` trait。

```
┌─────────────────────────────────────────────────────┐
│ CodecProvider (base.rs)                             │
│ 知道: reqwest, tokio::sync::mpsc, ProviderEvent     │
│ 职责: HTTP 发送, 错误处理, ChannelSink 桥接          │
└─────────────────────────────────────────────────────┘
                      ↓
┌─────────────────────────────────────────────────────┐
│ process_stream (stream_processor.rs)                │
│ 签名: S: Stream<Item=Result<Bytes,LlmError>>        │
│       E: EventSink  (async fn emit(StreamEvent))   │
│ 知道: bytes::Bytes, futures_core::Stream            │
│ 不知道: reqwest, tokio channel, ProviderEvent        │
└─────────────────────────────────────────────────────┘
                      ↓
┌─────────────────────────────────────────────────────┐
│ SseParser + ChatCodec + ToolCallAccumulator          │
│ 纯逻辑，无 IO                                       │
└─────────────────────────────────────────────────────┘
```

**`StreamEvent` — stream 模块对外的数据契约：**
```rust
pub(crate) enum StreamEvent {
    Start { model: String },
    Token { token: String },                        // 可丢弃
    ThinkingDelta { thinking, redacted },           // 可丢弃
    Error(LlmError),                                // 不可丢弃（关键）
    ResponseComplete { tool_calls, usage },         // 不可丢弃（关键）
}
```

**`EventSink` — 解耦输出端：**
```rust
pub trait EventSink {
    async fn emit(&mut self, event: StreamEvent) -> bool;  // false = 消费者断开
    fn is_closed(&self) -> bool { false }                  // 快速探测
}
```

**关键事件 vs 可丢弃事件：**
- `Error` 和 `ResponseComplete` 通过 `async send()` 阻塞等待送达
- `Token` 和 `ThinkingDelta` 通过 `try_send()` 非阻塞发送，channel 满时静默丢弃

**`process_stream` 泛型签名：**
```rust
pub async fn process_stream<S, A, E>(
    sink: &mut E,
    codec: &A,
    model: String,
    stream_thinking: bool,
    mut bytes_stream: S,
) where
    S: Stream<Item = Result<Bytes, LlmError>> + Unpin,
    A: ChatCodec,
    E: EventSink,
{
    // ...
}
```

**CodecProvider::stream() 中的调用：**
```rust
// 将 reqwest::Response 转换为通用字节流
let byte_stream = resp
    .bytes_stream()
    .map(|item| item.map_err(|e| LlmError::Network { detail: e.to_string() }));

let mut sink = ChannelSink { tx };
tokio::spawn(async move {
    process_stream(&mut sink, &codec, model, stream_thinking, Box::pin(byte_stream)).await;
});
```

**测试价值：**
```rust
// 无需 mockito / wiremock / reqwest::Response
let stream = futures_util::stream::iter(vec![
    Ok(Bytes::from("data: {\"text\": \"hel\"}\n\n")),
    Ok(Bytes::from("data: {\"text\": \"lo\"}\n\n")),
]);
process_stream(&mut mock_sink, &codec, "test".into(), false, stream).await;
```

**未来扩展：** 接入 hyper、aws-smithy、mock transport，`process_stream()` 完全不用改。

## 6. CodecProvider 已实现 LlmProvider

`CodecProvider<C: ProviderExtension + Clone>` 自动 `impl LlmProvider`。

**关键实现细节：**
- SSE 行缓冲由 `SseParser` 独立处理，`bytes_stream()` 的截断问题（跨 chunk 拼包）在 `SseParser` 内部解决
- `ToolCallAccumulator` 在 `stream_processor.rs` 中组装增量 delta
- `process_stream()` 通过 `EventSink` trait 输出事件，不知 reqwest / tokio channel
- `ChannelSink` 在 `base.rs` 中桥接 `EventSink` ←→ tokio channel + `ProviderEvent`
- `validate_request()` 在 `call()`/`stream()` 入口调用，校验消息语义 + 能力匹配
- 核心 trait（`ChatCodec` 等）全部 `pub`，外部可实现自定义 Provider

## 7. 恢复层 — RetryPolicy（瞬时故障）与 FallbackStrategy（路由决策）

**核心原则：** v0.1 发布前，仓库中不允许存在 "Runtime 永远不会调用到的恢复模块"。要么接入，要么标记 v0.2。

### 7.1 ToolError 类型化

**决定：** 删除 `ToolCallResult` 枚举，改用 Rust 原生 `Result`：

```rust
pub type ToolResult = Result<String, ToolError>;

pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

pub enum ToolErrorKind {
    NotFound,
    Timeout,
    Network,
    PermissionDenied,
    InvalidInput,
    RateLimited,
    LoopDetected,
    Internal,
}
```

**理由：**
- `ToolCallResult::Ok/Err(String)` 是纯字符串错误，`RetryPolicy` 无法分类
- Rust 已有 `Result<T,E>`，不需要再包一层枚举
- 类型安全：`match err.kind` 编译期 exhaustive check

### 7.2 RetryPolicy — 瞬时故障恢复（"再试一次"）

**执行链：** `ToolUseLoop → ToolExecutor → RetryPolicy → tool_fn()`

```rust
// ToolExecutor 内部
async fn execute(&self, call: &ToolCall) -> ToolResult {
    let tool_fn = self.tools.get(&call.name).expect("...");
    RetryPolicy::execute_with_retry(tool_fn).await
}
```

**可重试 vs 不可重试：**

| ToolErrorKind | 可重试 | 策略 |
|--------------|--------|------|
| NotFound | ❌ | 直接返回 |
| Timeout | ✅ | 指数退避 |
| Network | ✅ | 固定间隔 |
| RateLimited | ✅ | 按 Retry-After |
| PermissionDenied | ❌ | 直接返回 |
| InvalidInput | ❌ | 直接返回 |
| LoopDetected | ❌ | 直接返回 |
| Internal | ⚠️ | 视情况 |

**RetryPolicy 负责：** 是否重试、退避间隔、最大次数。
**FallbackStrategy 负责：** Retry 耗尽后，换条路走（Abort / SwitchProvider / AskUser）。

### 7.3 FallbackStrategy — 路由决策（"换条路走"）

**钩子点：**

| 触发条件 | 钩子位置 | FallbackReason |
|---------|---------|----------------|
| Provider 调用失败 | `call()` / `stream()` 返回 `Err` | `LlmError` |
| 工具重试耗尽 | RetryPolicy 返回 `Err` | `ToolError` |
| 连续 N 轮 ToolCall | LoopDetector 触发（v0.2） | `LoopDetected` |
| 达到最大迭代 | for 循环结束 | `MaxIterationsReached` |

**v0.1 实现范围：** ⚠️ 可选
- 只实现 `Retry` + `Abort`（`DefaultFallback`）
- Fallback 钩子只在 **Provider 错误** 处触发

**`FallbackContext` — 观察窗口，不持有错误所有权：**
- `error: &'a LlmError`（借用）— Context 只观察，不成为错误的临时仓库
- 错误所有权始终留在 Retry Loop 手中，Abort 时直接返回 owned `err`
- `execute_with_fallback<T, F, Fut>()` 自由函数统一处理 `execute()` 和 `execute_stream()` 的重试逻辑
- 零成本抽象 — 泛型 `F: FnMut() -> Fut`，无 `Box<dyn Future>`

**v0.2 扩展：**
- `SwitchProvider` — 传入 `Vec<ResolvedModel>` 备选链
- `RetryWithMessages` — 注入干预消息
- `LoopDetected` / `SignalVoter` 触发 Fallback

## 8. AgentEvent 流式阶段事件

**决定：** 流式模式下，ToolCall 必须在 `ProviderEvent::ResponseComplete` 后统一提交执行（原子执行）。

**`ResponseComplete` 语义（v2026-06-05 重命名）：**
- `ProviderEvent::Done` 曾承担两种语义：Provider 层"单次请求结束" vs Agent 层"推理结束"
- 重命名为 `ResponseComplete`，明确表示"单次 HTTP/SSE 请求完成"
- 消费者通过 `tool_calls.is_empty()` 判断模型是否给出最终答案

**事件流：**
```
Provider(Start)
Provider(Token)*
Provider(ResponseComplete { tool_calls: [...] })  ← 有工具调用
ToolStart / ToolEnd *
Provider(Start)
Provider(Token)*
Provider(ResponseComplete { tool_calls: [] })     ← 无工具调用
LoopEnd
```

**`ToolExecutionStart` / `ToolExecutionEnd` 是状态机层面的事件：**
- 表示 Agent 从 LLM 阶段切换到工具阶段
- 不是模型思考状态（不用 `ThinkingStart/End` 命名）
- 消费者可用此显示 "Executing N tools..."

**为什么原子执行而非即时执行：**
- v0.1 核心是 LLM ↔ Tool Call 闭环，不是低延迟流式
- 工具执行的 `ToolStart`/`ToolEnd` 与 `Token` 交错会让消费者解析更复杂
- 工具在 LLM 完整返回后执行，消费者逻辑简单

**工具执行并发策略（非流式 vs 流式）：**

| 模式 | Safe 工具 | CategoryExclusive | Exclusive |
|------|----------|-------------------|-----------|
| `execute()` 非流式 | ✅ 并发 (`join_all`) | ✅ 组内串行、组间并发 | ✅ 串行 |
| `execute_stream()` 流式 | ⚠️ 串行 | ⚠️ 串行 | ✅ 串行 |

流式串行是有意为之——`execute_stream()` 的核心价值是实时 Token 输出，工具执行是次要路径。v0.2 再优化流式分组并发。

**`execute_stream()` 已知问题：**
- ~~`ProviderEvent::Start` 重复发送~~ — ✅ 已修复（v2026-06-05）。Provider 发一次，Agent 统一透传，不再手动构造。
- ~~`ProviderEvent::Done` 语义歧义~~ — ✅ 已修复（v2026-06-05）。重命名为 `ResponseComplete`，明确表示"单次 HTTP/SSE 请求完成"。
- ~~`last_response` 外部状态管理~~ — ✅ 已修复（v2026-06-06）。`StreamIterResult` 改为枚举（Continue/Complete/Terminated），类型层面保证 Complete 携带最终响应。
- spawn 任务延迟终止 — 分层解决：
  - **v0.1 ✅** `ChannelSink::is_closed()` + `emit() -> bool` fast-exit。消费者断开后，下一次循环迭代或 `ResponseComplete` 发送前立即退出。
  - **v0.2** `CancellationToken` + `AbortHandle` + HTTP stream 立即关闭。

### spawn 任务取消 — 分层方案

| 层级 | 机制 | 效果 | 延迟窗口 |
|------|------|------|----------|
| v0.1 | `is_closed()` 快速探测 | 避免解析开销 | 最多 1 个 HTTP chunk |
| v0.1 | `emit() -> bool` fast-fail | 立即退出 | 0（发现即退出） |
| v0.2 | `CancellationToken` | 协作式取消 | 取决于取消点密度 |
| v0.2 | `AbortHandle` | 强制终止 spawn | 取决于 tokio yield 点 |

**v0.1 实现要点：**
- `EventSink::emit()` 返回 `bool`，`false` 表示消费者已断开
- `EventSink::is_closed()` 默认返回 `false`（测试 mock 无需覆盖）
- `ChannelSink` 实现 `is_closed()` → `tx.is_closed()`
- `process_stream` 在循环入口 + `ResponseComplete` 前检查 `is_closed()`

### `execute()` vs `execute_stream()` 等价性边界

流式模式手动构建 `ChatResponse`（`text_buffer` + `ToolCallAccumulator` + `usage`），非流式直接使用 Provider 返回的 `ChatResponse`。

| 字段 | v0.1 等价 | 说明 |
|------|----------|------|
| **Text** | ✅ 必须等价 | 流式累积的 Token 必须等于非流式的 Text |
| **ToolCall** | ✅ 必须等价 | `ToolCallAccumulator` 产出必须与非流式一致 |
| **Usage** | ✅ 必须等价 | Provider 最终 usage 必须传递到流式 ChatResponse |
| **raw** | ⚠️ 不要求等价 | 流式 `raw = null`（天然无单次完整响应） |
| **Thinking** | ✅ 必须等价 | 流式 `StreamChunk::ThinkingDelta` 已实现，`thinking_buffer` 累积到 `ChatResponse` |

**如果 Text / ToolCall / Usage 在两种模式下产出不同，属于 correctness bug，v0.1 必须修复。**

## 9. Message 语义校验

**决定：** `ContentBlock` 保持统一（Text / Thinking / Image / ToolCall），`Message::ToolResult` 使用 `Vec<ContentBlock>`。类型层面不限制，通过 `validate()` 方法施加语义约束。

**语义约束：**

| Message 变体 | 允许的 ContentBlock | 禁止的 ContentBlock |
|-------------|-------------------|-------------------|
| `User` | Text, Image | — |
| `Assistant` | Text, Thinking, ToolCall | — |
| `ToolResult` | Text, Image | ToolCall, Thinking |

**校验方式：**
```rust
impl Message {
    pub fn validate(&self) -> Result<(), LellmError> {
        match self {
            Message::ToolResult { content, .. } => {
                for block in content {
                    match block {
                        ContentBlock::ToolCall(_) | ContentBlock::Thinking(_) => {
                            return Err(LellmError::Parse(ParseError {
                                detail: "ToolResult must not contain ToolCall or Thinking blocks".into(),
                            }));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
```

**理由：**
- 不引入 `ToolResultContentBlock` 等冗余类型
- Core 保持最小、统一、易扩展
- 校验点在边界处执行（如 ToolUseLoop 组装 ToolResult 时）

## 10. build_request 序列化完整性

**问题：** `extract_text()` API 隐含了「所有 Message 都能降维成 String」的错误假设。

```rust
// 反模式 — Assistant 的 ToolCall 被丢弃
Message::Assistant { content: [ToolCall(...)] }
    .extract_text()  // → ""
```

这导致第二轮请求发给 OpenAI 时，`assistant(tool_call) → tool_result` 历史链断裂——**这是 correctness bug，不是 feature**。

### v0.1 最小正确性（必须）

| ContentBlock | 策略 | 理由 |
|-------------|------|------|
| **Text** | ✅ 完整支持 | 基础协议 |
| **Assistant ToolCall** | ✅ 完整支持 | Tool Use Loop 闭环的硬要求 |
| **ToolResult** | ✅ 完整支持 | 已有 `role: "tool"` + `tool_call_id` |
| **Thinking** | ⚠️ 静默忽略 | OpenAI 无对应字段，是 provider capability 问题 |

### v0.1 明确报错（未实现）

| ContentBlock | 策略 |
|-------------|------|
| **Image** | `Err(LlmError::UnsupportedFeature(...))` |
| **Audio / Video** | `Err(LlmError::UnsupportedFeature(...))` |

**比静默丢弃强得多。**

### v0.2 完整协议支持

- 多模态 User 消息（Image → `image_url` 数组格式）
- Thinking 映射到支持的 provider（如 OpenAI `reasoning_efforts`）
- Provider 能力协商

### 改造方向

**彻底移除 `extract_text()` 在 Adapter 请求构建路径中的使用。**

改为 Adapter 各自实现完整映射函数：

```rust
// OpenAI Adapter 内部
fn serialize_message(msg: &Message) -> Result<serde_json::Value, LlmError> {
    match msg {
        Message::User { content } => serialize_user(content),
        Message::Assistant { content } => serialize_assistant(content),
        Message::ToolResult { tool_call_id, content } => serialize_tool_result(tool_call_id, content),
        Message::System { content } => serialize_system(content),
    }
}
```

**理由：**
- `extract_text()` 天然无法表示 `Vec<ContentBlock>` 结构化消息
- OpenAI 与 Anthropic 的序列化规则不同，应在 Adapter 内部分开实现
- v0.1 必须保证 `Assistant(tool_call) → ToolResult` 多轮历史保真，否则 Agent Loop 不是真正闭环

## 11. 输出预算保险丝

**问题：** Agent 层缺少对 LLM 输出总量的控制。存在多种失控场景：

| 场景 | 原因 | 后果 |
|------|------|------|
| Provider 忽略 `max_tokens` | llama.cpp / vLLM / Ollama 等 OpenAI Compatible Server 实现不完整 | 单次响应无限长 |
| 代理层吞参数 | 中间代理未转发 `max_tokens` 字段 | 同上 |
| 多轮工具循环 | 每轮 4k × 10 轮 = 40k token | 总成本失控 |
| 模型无限思维链 | 模型进入超长 reasoning 模式 | CPU/内存/带宽/费用耗尽 |

**决定：** 两层保险丝，分别在流式消费层和 Agent Run 层。

### 11.1 单轮输出预算（P0）

`process_stream_iteration()` 在流式消费时实时累计 Token：

```
SSE Delta
  ↓
estimate_text(delta) → round_output_tokens += n
  ↓
round > max_output_tokens → OutputBudgetExceeded（立即切断流）
```

**关键设计：**
- 边接收边检查，不是等 `ResponseComplete` 才判断
- 使用 `estimate_text()`（CJK-aware 启发式）做增量估算
- 统计范围：`Token` + `ThinkingDelta`（不含 Tool Call 结构开销）
- 超限时构建 `build_partial_response()` 返回已接收的内容
- 返回 `StreamIterResult::OutputBudgetExceeded`，Agent 层立即停止

**为什么不用 `text.len()`：**
- `"hello world"` = 11 chars ≈ 3 tokens
- `"陆家嘴潍坊街道"` = 7 chars ≈ 10~15 tokens
- 差异巨大，必须用 Token 估算

### 11.2 总输出预算（P1）

`ToolUseConfig` 新增 `max_total_output_tokens: Option<u32>`：

```rust
pub struct ToolUseConfig {
    pub max_output_tokens: u32,           // 单轮 LLM 输出上限
    pub max_total_output_tokens: Option<u32>, // 整个 Agent Run 输出上限
    // ...
}
```

**执行流程：**
```
每轮 LLM 调用完成
  ↓
state.add_output_from_content(&response.content)
  ↓
state.total_output_tokens += estimate_content_tokens(content)
  ↓
total > max_total_output_tokens → OutputBudgetExceeded（停止 Agent）
```

**统计时机：**
- 非流式：`execute()` 在每次 `call()` 返回后累计
- 流式：`execute_stream()` 在 `Continue` / `Complete` 时累计

**Builder API：**
```rust
let agent = AgentBuilder::new(model)
    .max_output_tokens(16_000)        // 单轮上限
    .max_total_output_tokens(32_000)  // 总上限
    .build();
```

### 11.3 StopReason 枚举

```rust
pub enum StopReason {
    Complete,                  // Agent 已获得最终答案并正常结束
    MaxIterationsReached,      // 达到最大轮次
    Cancelled,                 // 外部取消（消费者断开、task 终止等）
    OutputBudgetExceeded,      // 输出预算超限（Text token）
    ReasoningBudgetExceeded,   // 推理预算超限（Thinking token）
}
```

**设计原则：** 每个停止原因语义独立，不复用。日志排查时，原因截然不同。

### 11.4 推理预算 — 对齐输出预算的双层设计

> **v2026-06-10 新增：** 推理预算与输出预算对称设计。

**背景：** 模型推理（Thinking）可能消耗大量 Token，需要独立于输出预算的控制。

**两层保险丝：**

| 层级 | 字段 | 作用域 | 默认值 |
|------|------|--------|--------|
| 单轮推理预算 | `ChatRequest.max_reasoning_tokens` | 单次 LLM 调用 | 无限制 |
| 总推理预算 | `ToolUseConfig.max_total_reasoning_tokens` | 整个 Agent Run | 无限制 |

**单轮推理预算：** 在流式消费时实时累计 `ThinkingDelta`，超过 `max_reasoning_tokens` 立即切断。
**总推理预算：** `LoopState.total_reasoning_tokens` 累计所有轮次，超过阈值返回 `ReasoningBudgetExceeded`。

**Builder API：**
```rust
let agent = AgentBuilder::new(model)
    .reasoning(ReasoningConfig::High)
    .reasoning_budget(8_000)            // 单轮推理上限
    .max_total_reasoning_tokens(32_000) // 总推理上限（可选）
    .build();
```

### 11.5 LoopState 输出 + 推理跟踪

`LoopState` 同时跟踪 Output（Text）和 Reasoning（Thinking）Token：

```rust
pub struct LoopState {
    // ... 现有字段
    pub total_output_tokens: usize,      // 累计输出 Token（Text）
    pub total_reasoning_tokens: usize,   // 累计推理 Token（Thinking）
}

impl LoopState {
    pub fn add_output_from_content(&mut self, content: &[ContentBlock]);
    // Text → total_output_tokens, Thinking → total_reasoning_tokens
    pub fn exceeded_total_output(&self, max: Option<u32>) -> bool;
    pub fn exceeded_total_reasoning(&self, max: Option<u32>) -> bool;
    pub fn finish_output_budget(&self, response: ChatResponse) -> ToolUseResult;
    pub fn finish_reasoning_budget(&self, response: ChatResponse) -> ToolUseResult;
}
```

### 11.6 Token 估算函数

`estimate_text()` 从 `context.rs` 导出为 `pub`：

```rust
pub fn estimate_text(s: &str) -> usize;
```

**估算规则：**
- ASCII 字符: 4 chars ≈ 1 token（BPE 常见比例）
- CJK 汉字: 2.5 tokens/字
- 其他 Unicode: 1 token/字
- 1.1x 安全系数

**未来扩展：** v0.2 可替换为 `TokenEstimator` trait + Provider-specific tokenizer（如 `tiktoken-rs`）。

### 11.7 保护层级总结

```
┌──────────────────────────────────────────────────────────┐
│ 第 1 层: ChatRequest.max_tokens → Provider 侧限制         │ ← 可能失效
├──────────────────────────────────────────────────────────┤
│ 第 2 层: process_stream_iteration 单轮预算 → 客户端切断    │ ← P0 保险丝
├──────────────────────────────────────────────────────────┤
│ 第 3 层: max_total_output_tokens → Agent Run 总预算       │ ← P1 保险丝
├──────────────────────────────────────────────────────────┤
│ 第 4 层: max_iterations → 轮次上限                        │ ← 已有
├──────────────────────────────────────────────────────────┤
│ 第 5 层: ContextBudget.max_tokens → 输入上下文上限         │ ← 已有
└──────────────────────────────────────────────────────────┘
```

即使 Provider 忽略 `max_tokens`、代理层吞参数、模型无限思维链 — Agent 仍有最后一道保险丝。

## 12. 日志降噪

**问题：** 流式推理路径上的高频日志淹没调试信息。

### 12.1 移除的高频日志

| 位置 | 原级别 | 触发频率 | 处理 |
|------|--------|----------|------|
| `stream_processor.rs` TCP chunk | `trace!` | 每个 TCP 包（数千次） | 移除 |
| `base.rs` channel 满丢弃 | `warn!` | 消费者慢时持续触发 | 改为静默（预期行为） |
| `stream_processor.rs` [DONE] frame | `trace!` | 每个 SSE frame | 移除 |

### 12.2 保留的日志

| 位置 | 级别 | 触发条件 |
|------|------|----------|
| `stream_processor.rs` stream error | `error!` | 流式读取错误 |
| `stream_processor.rs` parse error | `warn!` | 非 [DONE] 解析失败 |
| `base.rs` critical event lost | `error!` | 关键事件丢失 |
| `runtime.rs` output budget exceeded | `warn!` | 预算超限（罕见） |
| `runtime.rs` tool-use iteration | `debug!` | 每轮一次 |

### 12.3 示例代码清理

`tool_use_react.rs` 示例移除了所有 `[DEBUG]` 打印，只保留业务相关输出。ThinkingDelta 不再逐 token 打印到 stdout（避免与正常文本混排）。

## 13. 推理控制 — ReasoningConfig + stream_thinking 两层模型

### 背景

部分 LLM（如 OpenAI o 系列、DeepSeek R 系列、Qwen 推理版）支持"深度推理"模式，模型在输出最终答案前会先生成推理/思考过程。这需要两个正交的控制维度：

1. **是否允许模型推理** — 请求级别的参数，影响模型行为
2. **是否向客户端输出推理过程** — 流式级别的开关，影响事件发射

### 设计

#### `ReasoningConfig` 枚举

```rust
pub enum ReasoningConfig {
    Disabled, // 显式关闭推理
    Low,      // 低推理预算
    Medium,   // 中等推理预算
    High,     // 高推理预算
}
```

放在 `ChatRequest` 上（仅推理配置，不影响事件管道）：

```rust
pub reasoning: Option<ReasoningConfig>,
pub max_reasoning_tokens: Option<u32>,  // 单轮推理 Token 上限
```

#### 四值语义

| 值 | 含义 |
|---|---|
| `None` | 不干预，Provider 自行决定默认行为 |
| `Some(Disabled)` | 显式关闭推理（尽最大努力） |
| `Some(Low)` | 低推理预算（快速、轻量） |
| `Some(Medium)` | 中等推理预算 |
| `Some(High)` | 高推理预算（深度思考） |

**关键区分：** `None` ≠ `Some(Disabled)`。前者是"我不关心"，后者是"我不要"。

#### `stream_thinking` — 从 ChatRequest 移至 ToolUseConfig

> **v2026-06-07 重构：** `stream_thinking` 从 `ChatRequest` 移至 `ToolUseConfig`。

**原因：** `stream_thinking` 控制框架行为（Event 管道），不属于协议参数。Codec 不应看到此字段。

```rust
// ToolUseConfig
pub stream_thinking: bool;  // false = 不发射 ThinkingDelta, true = 发射
```

**过滤位置：** Agent 层 `process_stream_iteration()` 根据 `stream_thinking` 决定是否向消费者发射 ThinkingDelta。
Provider 层是无脑管道，永远转发所有协议事件。

典型组合：

| reasoning | stream_thinking | 场景 |
|---|---|---|
| `Some(Low)` | `false` | Agent Tool Loop — 后台轻量推理，不污染工具输出 |
| `Some(High)` | `true` | 用户想看完整思考过程 |
| `Some(Disabled)` | `false` | 明确不要推理，只要答案 |
| `None` | `false` | 让 Provider 自己决定 |

### Adapter 映射规则

**核心原则：** `Disabled` 对任何 Provider 都是"静默成功"。只有"请求了能力但 Provider 没有"才报 `UnsupportedFeature`。

| Provider | Disabled | Low/Medium/High |
|---|---|---|
| OpenAI Compatible | omit reasoning 字段 | `reasoning_effort="low/medium/high"` |
| DeepSeek | `enable_thinking=false` | `reasoning_effort=<level>` + `max_reasoning_tokens` |
| Anthropic | omit thinking 字段 | `thinking.type="enabled"` + `budget_tokens`（Low=2048, Medium=8192, High=32768） |
| 不支持推理的 Provider | 静默忽略 | `UnsupportedFeature` |

**Anthropic budget_tokens 优先级：** `max_reasoning_tokens` 存在时 → 覆盖默认值；不存在时 → 用 Config 对应的默认值。

**OpenAI 兼容协议注意：** OpenAI o 系列不支持 `max_reasoning_tokens` 的请求级传递，推理预算控制完全依赖客户端侧的流式保险丝（`process_stream_iteration` 中的 `max_reasoning_tokens` 检查）。

### 实现位置

- `ReasoningConfig` 枚举：`lellm-core/src/request.rs`
- `stream_thinking` 过滤：`lellm-provider/.../stream_processor.rs` — `process_stream()` 中根据标志决定是否发射 `ThinkingDelta`
- Codec 映射：各 Codec 的 `encode()` 中

## 14. RequestOptions — Agent 层生成参数覆盖

> **v2026-06-10 新增：** 独立于 ChatRequest 的 Agent 层参数覆盖。

**背景：** AgentBuilder 需要支持 temperature、top_p、seed、tool_choice 等生成参数的便捷设置。

**设计原则：**
- **解耦**：RequestOptions 定义自己的字段，不包裹 ChatRequest
- **选择性覆盖**：`apply()` 只覆盖非默认值（Some 字段）
- **Agent 保留字段**：`model`、`messages`、`tools` 由 Agent 层注入，`apply()` 跳过

```rust
pub struct RequestOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub stop_sequences: Option<Vec<String>>,
    pub prefill: Option<String>,
    pub reasoning: Option<ReasoningConfig>,
    pub max_reasoning_tokens: Option<u32>,
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

impl RequestOptions {
    pub fn apply(&self, req: &mut ChatRequest);  // 非默认值覆盖
}
```

**Builder API：**
```rust
AgentBuilder::new(model)
    .temperature(0.1)
    .reasoning(ReasoningConfig::High)
    .tool_choice(ToolChoice::Any)
    .build();
```

**`tool_choice` 仅首轮生效：** `build_request_inner_with_round()` 在 `iteration > 0` 时清除 `tool_choice`，让 LLM 自主决定。

## 15. Context Compaction — 上下文压缩

> **v2026-06-10 新增：** 可插拔的上下文压缩机制。

**背景：** 长对话场景下，消息历史不断增长，最终超出模型上下文窗口。需要在运行时自动压缩。

### 15.1 ContextBudget — 预算配置

```rust
pub struct ContextBudget {
    pub max_tokens: usize,              // 默认 128k
    pub warning_ratio: f32,             // 80% 触发压缩
    pub keep_recent_turns: usize,       // 保留最近 5 个 Turn
    pub max_tool_result_chars: usize,   // 单条工具结果最大 4096 字符
}
```

**v0.1：** 固定默认值 128k。
**v0.2：** 从 `ResolvedModel.context_window` 自动推导（window * 0.8）。

### 15.2 ContextCompactor — 可插拔策略

```rust
pub trait ContextCompactor: Send + Sync {
    fn compact(&self, messages: &[Message], budget: &ContextBudget) -> CompactionResult;
}

pub struct CompactionResult {
    pub messages: Vec<Message>,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub removed_messages: usize,
}
```

**关键约束：** Assistant(tool_call) + 对应的 ToolResult 是原子块，不可拆分。

### 15.3 LocalCompactor — v0.1 默认实现

**策略：**
1. 保留 System 消息
2. 按 Turn 分组（Assistant + ToolResult = 原子 Turn）
3. 保留最近 N 个 Turn
4. 旧 Turns 压缩为纯文本摘要

**摘要格式：**
```
[Previous conversation summary]
Compressed 3 turns:
  Assistant: The user asked about weather in Beijing
  Tool(get_weather): {"city": "beijing"}
  Result: Temperature is 22°C
  User: What about Shanghai?
```

- 纯文本标记（`Assistant:`/`Tool(...)`/`Result:`/`User:`），不使用 emoji
- 摘要注入为 `Message::System`，避免干扰对话结构
- Assistant 文本截 200 字符，Tool 参数截 100 字符，ToolResult 截 100 字符

### 15.4 压缩时机与事件

**时机：** 每轮迭代前，`estimated_tokens > max_tokens * warning_ratio` 时触发。

**事件：** 压缩完成后发射 `AgentEvent::ContextCompacted`：
```rust
AgentEvent::ContextCompacted {
    before_tokens: usize,
    after_tokens: usize,
    removed_messages: usize,
}
```

### 15.5 工具结果截断

**两条路径统一截断：** 流式和非流式执行路径均在工具结果注入历史前截断。

截断逻辑统一在 `LoopState.push_tool_results()` 中执行，两条路径（流式/非流式）在调用 `push_tool_results()` 时传入 `ContextBudget`，由该方法统一处理截断。确保所有进入历史的工具结果受 `max_tool_result_chars` 控制。

## 16. 门面 Crate Feature Gate

> **v2026-06-10 更新：** provider 作为默认 feature。

```toml
[features]
default = ["provider"]
core = ["dep:lellm-core"]
provider = ["dep:lellm-core", "dep:lellm-provider"]
agent = ["dep:lellm-core", "dep:lellm-agent"]
macros = ["dep:lellm-macros"]
full = ["provider", "agent", "macros"]
```

**用户体验：** `cargo add lellm` 即可获得 core + provider。需要 Agent 运行时加 `--features agent`。
