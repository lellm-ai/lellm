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
│  ResolvedModel + ToolExecutor + ToolUseLoop         │
└─────────────────────────────────────────────────────┘
```

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
    max_iterations: usize,
    fallback: Arc<dyn FallbackStrategy>,
    system_prompt: Option<String>,  // Runtime Config
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
- `system_prompt` — 配置与状态分离，请求时组合

### 4. system_prompt 注入规则

```rust
// 请求时组合：config.system_prompt + state.messages
fn build_request_messages(&self, messages: &[Message]) -> Result<Vec<Message>, LlmError> {
    if let Some(ref sp) = self.system_prompt {
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

## 并行安全分级

| 级别 | 执行策略 |
|------|----------|
| `Safe` | 全部并发（join_all） |
| `CategoryExclusive` | 按 category 分组，组内串行、组间并发 |
| `Exclusive` | 全部串行 |

## 错误处理

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
