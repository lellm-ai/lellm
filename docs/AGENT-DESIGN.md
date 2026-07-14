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
| Config | `ToolUseConfig` | `system_prompt`, `max_iterations`, `max_output_tokens`, `max_total_output_tokens`, `context_budget` | `Clone + Send + Sync + Debug`，纯数据 |
| Deps | `ToolUseDeps` | `fallback: Arc<dyn FallbackStrategy>` | 策略服务，`Arc` 包裹 |

构造时：
```rust
ToolUseLoop::new(model, executor, config, deps)
```

便捷构造（默认值）：
```rust
ToolUseLoop::simple(model, executor)
```

**设计原则：**
- `AgentBuilder` = 唯一的配置入口
- `ToolUseLoop` = Runtime，不提供 `with_` 方法
- **不存在** `AgentBuilder::from_loop()` — 不鼓励 Runtime → Builder 的反向转换

## ReAct 图结构 — 固定 vs 可配置

> **v2026-07-14 决策：** ReAct 图结构固定，通过钩子扩展。

### 固定结构

`AgentBuilder.build()` 内部固定构建以下节点：

```
budget_check → llm → post_llm_check → tool → (回到 llm)
                    ↘
               compactor (上下文压缩)
```

用户**不能**替换 `LLMNode`、`ToolNode` 或修改循环结构。

### 扩展点（钩子）

已有的钩子覆盖了 80% 的自定义需求：

| 钩子 | 作用域 | 自定义能力 |
|------|--------|-----------|
| `FallbackStrategy` | LLM 调用失败 | 降级 / 切换 Provider / 中止 |
| `RetryPolicy` | 工具执行 | 重试次数、退避策略 |
| `ContextBudget` | 上下文管理 | Token 上限、压缩策略 |
| `ToolCatalog` | 工具来源 | 静态 / MCP 动态 / 组合目录 |
| `StepCallback` | 执行通知 | 每步执行事件 |
| `RequestOptions` | 请求参数 | temperature、seed、tool_choice 等 |

### 完全自定义

需要修改图结构的用户，直接使用 `GraphBuilder`（世界二）手写 ReAct 流程。

### 为什么不开放节点替换

1. **80/20 原则** — 80% 用户只需要标准 ReAct，固定结构最简单可靠
2. **避免过度配置** — 节点替换会让 AgentBuilder 膨胀为"杀牛刀"
3. **已有钩子足够** — 上面的钩子覆盖了常见自定义场景
4. **Graph Primitive 兜底** — 世界二提供完全自由

### 输出预算分层

`ToolUseConfig` 提供两层输出预算控制：

| 字段 | 默认值 | 作用域 | 说明 |
|------|--------|--------|------|
| `max_output_tokens` | 4,000 | 单次 LLM 调用 | 注入 `ChatRequest.max_tokens`，流式消费时实时检查 |
| `max_total_output_tokens` | `None` | 整个 Agent Run | 累计所有轮次输出，防止多轮成本失控 |

```rust
let agent = AgentBuilder::new(model)
    .max_output_tokens(16_000)        // 单轮上限
    .max_total_output_tokens(32_000)  // 总上限（可选）
    .build();
```

**保护层级：**
```
ChatRequest.max_tokens → Provider 侧限制（可能失效）
  ↓
process_stream_iteration 单轮预算 → 客户端切断（P0 保险丝）
  ↓
max_total_output_tokens → Agent Run 总预算（P1 保险丝）
  ↓
max_iterations → 轮次上限（已有）
  ↓
ContextBudget.max_tokens → 输入上下文上限（已有）
```

## 状态变更模型 — Mutation as Runtime IR

> **v2026-07-14 决策：** Mutation 是 Runtime 内部 IR，不是业务层 API。

### 核心原则

**Mutation 是 Graph Runtime 的唯一状态变更模型，但业务层不直接编写 Mutation。**

```
业务代码永远写 State 操作
Runtime 内部永远走 Mutation
```

### 层次划分

```
Graph Runtime
────────────────────
ExecutionLoop / Checkpoint / Reducer / Barrier / Trace
只认识 Mutation
        │
        ▼
State Context
────────────────────
append_message() / set_response() / increase_token() / record(...)
业务 API — 内部生成 Mutation
        │
        ▼
WorkflowState
────────────────────
apply(mutation) / snapshot() / restore()
完全不知道 Graph
        │
        ▼
AgentState
────────────────────
messages / tokens / iterations / stop_reason
纯数据，无逻辑
```

### 业务代码写法

```rust
// 现在（临时）
state.messages.push(msg)

// 目标（C2）
ctx.append_message(msg)
ctx.set_last_response(response)
ctx.add_tokens(n)
```

`ctx` 内部：

```
mutation_log.push(AppendMessage(msg))
state.apply(AppendMessage(msg))
```

### Mutation 设计

**每个 Mutation 一个类型**，实现 `StateMutation<S>` trait：

```rust
trait StateMutation<S> {
    fn apply(self, state: &mut S);
}

struct AppendMessage(Message);
impl StateMutation<AgentState> for AppendMessage { ... }

struct IncreaseIteration(u32);
impl StateMutation<AgentState> for IncreaseIteration { ... }
```

**不使用 enum** — 扩展时无需修改枚举体，符合开闭原则。

### WorkflowState trait 调整

```rust
// 现在
fn apply_batch(&mut self, mutations: impl IntoIterator<Item = Self::Mutation>);

// 目标
fn apply(&mut self, mutation: Self::Mutation);
// batch 是 Runtime 的事，State 只负责单个 apply
```

### 为什么保留 Mutation（不选 B）

| 能力 | 无 Mutation | 有 Mutation |
|------|-----------|------------|
| Checkpoint | 存整个 State | 记录 Mutation 序列 |
| Replay | 无法增量回放 | Mutation 序列回放 |
| Rollback | 需要完整快照 | 撤销最后 N 个 Mutation |
| Branch | 深拷贝 State | fork mutation log |
| Parallel Merge | merge(StateA, StateB) — 难 | merge(mutations_a, mutations_b) — Reducer 解决 |
| Barrier | collect State | collect mutations → apply → continue |
| Trace | Snapshot diff — 成本高 | Mutation log — 天然审计 |

### 为什么不让业务写 Mutation（不选 C）

- `AgentMutation::AddMessage(...)` 到处都是 → 业务代码噪音大
- Mutation 是 Runtime 的内部语言（IR），不应泄露到业务层
- 通过 `ctx.record()` 封装，业务代码保持简洁

### 实施路径

1. **v0.5（现在）** — Mutation trait 存在，AgentState 直接修改字段（临时兼容）
2. **v0.6** — 引入 `StateContext`，业务改为 `ctx.append_message()` 等 API
3. **v0.6** — Mutation 自动生成（可选 derive 宏）
4. **v0.7+** — 基于 Mutation 的 Checkpoint、Replay、Trace 完整落地

### 1. ToolRegistration — 工具完整条目

```rust
pub struct ToolRegistration {
    pub(crate) definition: ToolDefinition,  // JSON Schema
    pub(crate) safety: ParallelSafety,      // 并行安全分级
    pub(crate) category: Option<ToolCategory>,
    pub(crate) func: ToolFn,                // 执行函数
}

#[derive(Clone)]
pub struct ToolExecutor {
    catalog: Arc<dyn ToolCatalog>,
    retry_policy: RetryPolicy,
}
```

**设计原则**：Schema、安全分级、执行函数合一，消除数据泥团（4 个平行 HashMap）。

### 2. ToolRegistration — 工具注册

```rust
pub struct ToolRegistration {
    pub(crate) definition: ToolDefinition,
    pub(crate) safety: ParallelSafety,
    pub(crate) category: Option<ToolCategory>,
    pub(crate) func: ToolFn,
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

## ToolUseConfig 完整字段

```rust
pub struct ToolUseConfig {
    pub system_prompt: Option<String>,         // 系统提示
    pub max_iterations: usize,                 // 最大迭代轮次（默认 10）
    pub max_output_tokens: u32,                // 单轮输出上限（默认 4k）
    pub max_total_output_tokens: Option<u32>,  // 整个 Run 输出上限（默认无限制）
    pub context_budget: ContextBudget,         // 上下文预算管理
}
```

**Builder API：**
```rust
let agent = AgentBuilder::new(model)
    .system_prompt("你是一个有帮助的助手。".into())
    .tool(search_tool)
    .max_iterations(20)
    .max_output_tokens(16_000)
    .max_total_output_tokens(32_000)
    .build();
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

### StopReason 枚举

```rust
pub enum StopReason {
    Complete,              // Agent 已获得最终答案并正常结束
    MaxIterationsReached,  // 达到最大轮次
    Cancelled,             // 外部取消（消费者断开、task 终止等）
    OutputBudgetExceeded,  // 输出预算超限（单轮或总输出 token 超过限制）
    ReasoningBudgetExceeded,  // 推理预算超限（thinking tokens 超过限制）
}
```

**`OutputBudgetExceeded` 触发条件：**
- 流式消费时，单轮 `round_output_tokens > max_output_tokens`（Provider 忽略 max_tokens）
- Agent Run 累计 `total_output_tokens > max_total_output_tokens`（多轮成本失控）

**不复用 `MaxIterationsReached`** — 语义不同，日志排查时原因截然不同。

### 流式输出预算检查

流式模式下，`process_stream_iteration()` 在 `Token` 和 `ThinkingDelta` 事件时实时累计：

```
SSE Delta
  ↓
estimate_text(delta) → round_output_tokens += n
  ↓
round > max_output_tokens → StreamIterResult::OutputBudgetExceeded
  ↓
Agent 层收到 → LoopEnd { stop_reason: OutputBudgetExceeded }
```

边接收边切断，不等 `ResponseComplete`。

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
