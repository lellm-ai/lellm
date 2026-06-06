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
GenericProvider      ← 只读取 ChatRequest.model
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

## 5. ProviderAdapter SPI / ProviderRequest 中间层

**决定：** `ProviderAdapter` 完全不知道 `ProviderConfig` 和 `reqwest`。通过 `ProviderRequest` 中间层解耦。

```rust
pub(crate) struct ProviderRequest {
    pub path: Cow<'static, str>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn build_request(&self, req: &ChatRequest, stream: bool) -> Result<ProviderRequest, LlmError>;
    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;
    fn parse_sse_frame(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;
}
```

**`GenericProvider` 持有 config + client，统一组装 HTTP 请求：**
```rust
pub struct GenericProvider<A: ProviderAdapter> {
    adapter: A,
    config: ProviderConfig,
    client: reqwest::Client,
}
```

**职责切分：**

| 职责 | Adapter | stream/ 模块 | GenericProvider |
|------|---------|-------------|-----------------|
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

**`ProviderConfig` 精简为连接配置（不含 model）：**
```rust
pub struct ProviderConfig {
    pub base_url: url::Url,
    pub auth: AuthConfig,
    pub timeout: std::time::Duration,
}
```

### AuthConfig — apply() 替代 get_header()

**问题：** `get_header()` 将 `SecretString` 展开为明文 `String`，意外传播认证细节。

**决定：** 删除 `get_header()`，改为 `apply(builder) -> builder`：

```rust
impl AuthConfig {
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            AuthConfig::Bearer { api_key } => {
                builder.bearer_auth(api_key.expose_secret())
            }
            AuthConfig::Header { header, value } => {
                builder.header(header, value.expose_secret())
            }
            AuthConfig::None => builder,
        }
    }
}
```

**好处：**
1. **Secret 生命周期最短** — `expose_secret()` 在 `header()` 内部直接消费，不经过中间变量
2. **不暴露认证细节** — `GenericProvider` 不需要知道 `Authorization`、`Bearer` 等协议细节
3. **未来可扩展** — 支持 `AuthConfig::OAuth` 时只需加一个变体，API 不变

## 5.1 SSE 解析 — SseFrame 中间表示

**决定：** `SseParser` 负责 SSE 协议解析（行缓冲、`data:` 提取、空行检测），构建 `SseFrame` 交给 Adapter 解析 JSON payload。

```rust
/// SSE 帧 — SseParser 从字节流中构建，Adapter 只解析 data 字段。
pub(crate) struct SseFrame {
    /// event 字段（可选），如 "message_start", "content_block_delta"
    pub event: Option<String>,
    /// data 字段内容（通常是 JSON 字符串或标记如 "[DONE]"）
    pub data: String,
}
```

**签名：**
```rust
fn parse_sse_frame(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;
```

**理由：**
- Adapter 完全不知道 SSE 协议细节，只关心 `event` 类型和 `data` 内容
- `event` 字段对 Anthropic 等 provider 有用（区分 `message_start` / `content_block_delta` / `message_stop`）
- OpenAI 的 `[DONE]` 直接出现在 `data` 字段中，Adapter 自行判断
- SSE 行缓冲只在 `SseParser` 一处实现，可独立测试

**SseParser 的解析逻辑：**
```rust
// 1. 字节 → 字符串，按 \n 分割
// 2. 提取 event:xxx / data:xxx → SseFrame { event, data }
// 3. 空行表示 SSE 帧边界，提交 SseFrame
// 4. 不完整的帧保留在 buffer 中，等待下一块数据
```

### 5.2 流式处理 — 传输层解耦（EventSink + StreamEvent）

**问题：** `process_stream()` 直接接收 `reqwest::Response`，耦合 HTTP 客户端。测试需要 mockito/wiremock。

**决定：** `process_stream()` 只认识 `Stream<Item = Result<Bytes, LlmError>>` 和 `EventSink` trait。

```
┌─────────────────────────────────────────────────────┐
│ GenericProvider (base.rs)                           │
│ 知道: reqwest, tokio::sync::mpsc, ProviderEvent     │
│ 职责: HTTP 发送, 错误处理, ChannelSink 桥接          │
└─────────────────────────────────────────────────────┘
                      ↓
┌─────────────────────────────────────────────────────┐
│ process_stream (stream_processor.rs)                │
│ 签名: S: Stream<Item=Result<Bytes,LlmError>>        │
│       E: EventSink  (fn emit(StreamEvent))          │
│ 知道: bytes::Bytes, futures_core::Stream            │
│ 不知道: reqwest, tokio channel, ProviderEvent        │
└─────────────────────────────────────────────────────┘
                      ↓
┌─────────────────────────────────────────────────────┐
│ SseParser + Adapter + ToolCallAccumulator            │
│ 纯逻辑，无 IO                                       │
└─────────────────────────────────────────────────────┘
```

**`StreamEvent` — stream 模块对外的数据契约：**
```rust
pub(crate) enum StreamEvent {
    Start { model: String },
    Token { token: String },
    Error(LlmError),
    Done { tool_calls: Vec<ToolCall>, usage: Option<TokenUsage> },
}
```

**`EventSink` — 解耦输出端：**
```rust
pub trait EventSink {
    fn emit(&mut self, event: StreamEvent);
}
```

**`ChannelSink` — 桥接 EventSink ←→ tokio channel：**
```rust
struct ChannelSink {
    tx: tokio::sync::mpsc::Sender<Result<ProviderEvent, LlmError>>,
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: StreamEvent) {
        let _ = self.tx.try_send(map_stream_event(event));
    }
}
```

**`process_stream` 泛型签名：**
```rust
pub async fn process_stream<S, A, E>(
    sink: &mut E,
    adapter: &A,
    model: String,
    mut bytes_stream: S,
) where
    S: Stream<Item = Result<Bytes, LlmError>> + Unpin,
    A: ProviderAdapter,
    E: EventSink,
{
    // ...
}
```

**GenericProvider::stream() 中的调用：**
```rust
// 将 reqwest::Response 转换为通用字节流
let byte_stream = resp
    .bytes_stream()
    .map(|item| item.map_err(|e| LlmError::Network { detail: e.to_string() }));

let mut sink = ChannelSink { tx };
tokio::spawn(async move {
    process_stream(&mut sink, &adapter, model, Box::pin(byte_stream)).await;
});
```

**测试价值：**
```rust
// 无需 mockito / wiremock / reqwest::Response
let stream = futures_util::stream::iter(vec![
    Ok(Bytes::from("data: {\"text\": \"hel\"}\n\n")),
    Ok(Bytes::from("data: {\"text\": \"lo\"}\n\n")),
]);
process_stream(&mut mock_sink, &adapter, "test".into(), stream).await;
```

**未来扩展：** 接入 hyper、aws-smithy、mock transport，`process_stream()` 完全不用改。

## 6. GenericProvider 已实现 LlmProvider

`GenericProvider<A: ProviderAdapter + Clone>` 自动 `impl LlmProvider`。

**关键实现细节：**
- SSE 行缓冲由 `SseParser` 独立处理，`bytes_stream()` 的截断问题（跨 chunk 拼包）在 `SseParser` 内部解决
- `ToolCallAccumulator` 在 `stream_processor.rs` 中组装增量 delta
- `process_stream()` 通过 `EventSink` trait 输出事件，不知 reqwest / tokio channel
- `ChannelSink` 在 `base.rs` 中桥接 `EventSink` ←→ tokio channel + `ProviderEvent`
- `ProviderAdapter` 是 `pub(crate)`，外部只能通过 `LlmProvider` trait 使用

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
