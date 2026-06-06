# LeLLM Agent 设计文档

## 概述

LeLLM Agent 运行时提供完整的 LLM ↔ 工具调用闭环（ReAct 循环），支持非流式和流式两种模式。

## 架构分层

```
┌─────────────────────────────────────────────────────┐
│  第三层：生态包 (lellm-openai, lellm-anthropic)      │  ← 未来
│  create_agent(openai("gpt-5"), [search, weather])   │
├─────────────────────────────────────────────────────┤
│  第二层：AgentBuilder (推荐入口)                      │  ← P3
│  AgentBuilder::new(model).tool(...).build()         │
├─────────────────────────────────────────────────────┤
│  第一层：Runtime Core                               │  ← 已实现
│  ToolUseLoop { model, executor, config, deps }      │
└─────────────────────────────────────────────────────┘
```

### Config vs Deps 分层

`ToolUseLoop` 内部严格分离配置与依赖：

| 层级 | 结构体 | 内容 | 特性 |
|------|--------|------|------|
| Config | `ToolUseConfig` | `system_prompt`, `max_iterations` | `Clone + Send + Sync + Debug`，纯数据 |
| Deps | `ToolUseDeps` | `fallback: Arc<dyn FallbackStrategy>` | 策略服务，`Arc` 包裹 |

构造时：
```rust
ToolUseLoop::new(model, executor, config, deps)
```

便捷构造（默认值）：
```rust
ToolUseLoop::simple(model, executor)
```

次要入口（链式 with_）：
```rust
ToolUseLoop::new(...)
    .with_system_prompt("...".into())
    .with_max_iterations(20)
```

**设计原则：**
- `AgentBuilder` = 唯一推荐的配置入口
- `ToolUseLoop` = Runtime，`with_` 方法仅供高级用户微调
- **不存在** `AgentBuilder::from_loop()` — 不鼓励 Runtime → Builder 的反向转换

## 核心组件

### 1. ToolEntry — 工具完整条目

```rust
pub struct ToolEntry {
    pub definition: ToolDefinition,  // JSON Schema
    pub safety: ParallelSafety,      // 并行安全分级
    pub category: Option<ToolCategory>,
    pub func: ToolFn,                // 执行函数
}

#[derive(Clone)]
pub struct ToolExecutor {
    tools: Arc<HashMap<String, ToolEntry>>,
    retry_policy: RetryPolicy,
}
```

**设计原则**：Schema、安全分级、执行函数合一，消除数据泥团（4 个平行 HashMap）。

### 2. ToolRegistration — 工具注册

```rust
pub struct ToolRegistration {
    pub definition: ToolDefinition,
    pub safety: ParallelSafety,
    pub category: Option<ToolCategory>,
    pub func: ToolFn,
}

impl ToolRegistration {
    pub fn safe(def: ToolDefinition, handler) -> Self
    pub fn category_exclusive(def: ToolDefinition, category, handler) -> Self
    pub fn exclusive(def: ToolDefinition, handler) -> Self
}
```

### 3. ToolUseLoop — Agent 循环

```rust
#[derive(Clone)]
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    config: ToolUseConfig,
    deps: ToolUseDeps,
}

impl ToolUseLoop {
    // 借用，不消费 — 支持复用
    pub async fn execute(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError>
    pub fn execute_stream(&self, messages: Vec<Message>) -> AgentStream
}
```

**关键特性**：
- `#[derive(Clone)]` — 内部全 Arc，O(1) clone
- `&self` — 支持并发 execute
- `config` / `deps` 分层 — 配置与策略分离

### 4. system_prompt 注入规则

```rust
// 请求时组合：config.system_prompt + state.messages
fn build_request_messages(&self, messages: &[Message]) -> Result<Vec<Message>, LlmError> {
    if let Some(ref sp) = self.config.system_prompt {
        if has_system_message(messages) {
            return Err(LlmError::DuplicateSystemPrompt);  // 显式报错
        }
        // 组合...
    }
}
```

**规则**：
- 配置即配置，状态即状态
- 双来源冲突 → 报错，绝不静默覆盖
- 请求时组合，不修改原始 messages

### 5. 工具 Schema 自动注入

```rust
// ToolUseLoop 每次构建请求时自动注入
let req = ChatRequest {
    model: self.model.model.clone(),
    messages: state.messages.clone(),
    tools: self.executor.has_tools().then(|| self.executor.definitions()),
    ..Default::default()
};
```

链路：`注册工具 → ToolExecutor 持有 Schema → ToolUseLoop 自动注入 → Provider Adapter 转换 → LLM 收到 Tool Schema`

## ChatRequest 扩展

```rust
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,    // 新增
    pub top_p: Option<f64>,          // 新增
    pub seed: Option<u64>,           // 新增
    pub tool_choice: Option<ToolChoice>,
    pub stop_sequences: Option<Vec<String>>,
    pub prefill: Option<String>,
    pub extra: Option<serde_json::Map<...>>,  // Provider 特有参数
}
```

## derive(ToolDefinition) 宏

```rust
#[derive(ToolDefinition)]
#[tool(name = "search", description = "搜索互联网信息")]
pub struct SearchArgs {
    /// 搜索关键词
    pub query: String,
}

// 自动生成：
impl SearchArgs {
    pub fn __schema() -> serde_json::Value { ... }
    pub fn __name() -> &'static str { "search" }
    pub fn __description() -> &'static str { "搜索互联网信息" }
}
```

## 执行流程

```
用户消息
  ↓
ToolUseLoop.execute(&self, messages)
  ↓
build_request_messages() → system_prompt + messages
  ↓
build_request() → ChatRequest { tools: executor.definitions(), ... }
  ↓
provider.call(&req) → ChatResponse
  ↓
has_tool_calls()?
  ├─ No → 正常结束
  └─ Yes → executor.execute_batch(&tool_calls)
              ↓
            追加 Assistant + ToolResult 到 messages
              ↓
            继续循环
```

## 流式事件契约

```
AgentEvent::Provider(ProviderEvent::Token { ... })  // 文本增量
AgentEvent::Provider(ProviderEvent::ThinkingDelta { ... })  // 思考增量
AgentEvent::ToolStart { tool_call_id, name }        // 工具开始
AgentEvent::ToolEnd { tool_call_id, result }         // 工具结束
AgentEvent::LoopEnd { result }                       // 正常结束（恰好一次）
AgentEvent::LoopError { error, iterations }          // 异常结束（恰好一次）
```

**终态契约**：
- 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
- 异常结束：`LoopError` 恰好一次，然后 channel 关闭
- 终态事件后不再发送任何事件

### 流式工具执行策略

**v0.1 决策：** 流式模式工具执行强制串行，即使工具标记为 `Safe`。

原因：ToolStart/ToolEnd 与 Token 交错会让消费者解析更复杂。

**v0.2 计划：** 对 `ParallelSafety::Safe` 的工具在流式模式下做并发执行，事件顺序用 `tool_call_id` 区分。

## 并行安全分级

| 级别 | 执行策略 |
|------|----------|
| `Safe` | 全部并发（join_all）— 非流式模式 |
| `CategoryExclusive` | 按 category 分组，组内串行、组间并发 — 非流式模式 |
| `Exclusive` | 全部串行 |

> 注：流式模式下 v0.1 统一串行，v0.2 按级别优化。

## 错误处理

### LellmError — 门面层统一错误出口

```rust
/// 供 lellm facade crate 聚合各子层错误。
/// Core 层 API 不使用此类型。
pub enum LellmError {
    Llm(#[from] LlmError),
    Tool(#[from] ToolError),
    Memory(#[from] MemoryError),
    Parse(#[from] ParseError),
}
```

**分层策略：**
- Core 层 → 返回领域错误（`LlmError`, `ToolError`, `MemoryError`, `ParseError`）
- Agent 层 → 未来可聚合为 `AgentError` 或直接使用 `LellmError`
- Facade crate → 统一导出 `LellmError` 作为最终用户入口

### FallbackStrategy — Provider 降级

```rust
pub enum FallbackAction {
    Retry,   // 重试同一请求
    Abort,   // 终止并返回错误
}

pub struct DefaultFallback {
    max_retries: usize,  // 默认 3
}
```

可重试错误：Timeout, Network, 5xx ApiError

### RetryPolicy — 工具重试

```rust
pub struct RetryPolicy {
    max_attempts: u32,           // 总尝试次数（初始 + 重试）
    backoff: BackoffStrategy,    // Fixed / Exponential
}
```

可重试错误：Timeout, Network, RateLimited

## Message::ToolResult is_error

`Message::ToolResult` 携带 `is_error: bool` 标记，区分工具成功与失败：

```rust
Message::ToolResult {
    tool_call_id: String,
    is_error: bool,      // true = 工具执行失败
    content: Vec<ContentBlock>,
}
```

- 成功 → `is_error: false`，content 为工具返回值
- 失败 → `is_error: true`，content 为 `"tool error: {e}"`

Provider Adapter 序列化时映射到各 API 的 `is_error` 字段（Anthropic 支持，OpenAI 隐式通过内容传达）。

## 未来规划

### P3: AgentBuilder
```rust
let agent = AgentBuilder::new(model)
    .system_prompt("你是一个有帮助的助手。")
    .tool(search)
    .tool(weather)
    .max_iterations(20)
    .build();
```

### P1.5: ToolArgs trait + schemars
```rust
pub trait ToolArgs: DeserializeOwned + JsonSchema {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    fn tool_definition() -> ToolDefinition { ... }
}
```

### 第三层：生态包
```rust
let agent = create_agent(
    openai("gpt-5"),
    [search, weather],
)?;
```
