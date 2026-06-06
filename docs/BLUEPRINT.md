# lellm v0.1 产品蓝图

> 版本：v0.1 | 日期：2026-06-03 | 状态：代码已对齐
> 设计决策详见 [DESIGN.md](./DESIGN.md)

## 一、项目愿景

做 Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，标准化消息内容格式；提供基础的 LLM provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration（v0.2+）
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 二、v0.1 范围

### 包含

| Crate | 职责 | 核心内容 |
|-------|------|----------|
| `lellm` | 门面 crate | Feature-gated re-export 所有子 crate；用户统一入口 |
| `lellm-core` | 协议对象 | Message, ContentBlock, ChatRequest/Response, ToolDefinition, TokenUsage, LlmError |
| `lellm-provider` | Provider trait + 适配器 | LlmProvider, ProviderAdapter, GenericProvider, ModelRouter, ProviderRegistry, MockProvider |
| `lellm-agent` | Agent 运行时 | ToolExecutor, ToolUseLoop, AgentEvent, ParallelSafety, ToolRegistry, RetryPolicy, FallbackStrategy, ShortTermMemory |
| `lellm-macros` | 派生宏 | `#[derive(ToolDefinition)]` — Stub |

### 不包含（v0.2+）

- Graph/Node/Edge 编排层
- MCP Client/Server（v0.1 仅预留 `ToolSource::Mcp`）
- Sandbox / Harness Orchestrator
- LongTermMemory / MemoryStore

## 三、Workspace 结构

```
lellm/
├── Cargo.toml                  # workspace root
├── lellm/                      # 门面 crate — feature-gated re-export
├── lellm-core/                 # 协议对象，零运行时依赖
├── lellm-provider/             # Provider trait + 适配器
├── lellm-agent/                # Agent 运行时
└── lellm-macros/               # 派生宏
```

## 四、核心 API

### 4.1 LlmProvider

```rust
pub trait LlmProvider: Send + Sync {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError>;
    fn provider_id(&self) -> &str;
}
```

### 4.2 ToolUseLoop

```rust
// 推荐入口 — AgentBuilder
let agent = AgentBuilder::new(model)
    .system_prompt("...".into())
    .tool(search_tool)
    .max_iterations(20)
    .build();

// 内部结构 — Config vs Deps 分层
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    config: ToolUseConfig,    // 纯参数 (system_prompt, max_iterations)
    deps: ToolUseDeps,        // 策略服务 (fallback)
}

impl ToolUseLoop {
    pub fn new(model, executor, config, deps) -> Self;
    pub fn simple(model, executor) -> Self;  // 默认配置
    pub fn with_system_prompt(self, prompt) -> Self;  // 链式微调
    pub fn with_max_iterations(self, max) -> Self;
    // &self 借用，不消费 — 支持复用
    pub async fn execute(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError>;
    pub fn execute_stream(&self, messages: Vec<Message>) -> AgentStream;
}
```

### 4.3 ContentBlock

```rust
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    Image { source: ImageSource },
    ToolCall(ToolCall),
}
```

Message 使用 `Message::ToolResult` 变体携带工具执行结果，不混入 ContentBlock。
`ToolResult` 携带 `is_error: bool` 标记，区分工具成功与失败。

### 4.4 Provider 三层架构

```
用户 → LlmProvider (public API)
       → GenericProvider<A> (框架内部)
          → ProviderAdapter (pub(crate) SPI)
```

详见 [DESIGN.md §5](./DESIGN.md#5-provideradapter-spi--providerrequest-中间层)

## 五、关键设计决策索引

| 主题 | 详见 |
|------|------|
| 门面 crate 与 Feature Gate | [DESIGN.md §0](./DESIGN.md#0-lellm-门面-crate) |
| MaxIterationsReached 归 Ok | [DESIGN.md §1](./DESIGN.md#1-maxiterationsreached--ok-还是-err) |
| AgentEvent 终态契约 | [DESIGN.md §2](./DESIGN.md#2-agentevent-终态契约) |
| ToolUseLoop 与 model 单向流动 | [DESIGN.md §3](./DESIGN.md#3-tooluseloop-不知道-router--registry--model-单向流动) |
| ProviderAdapter SPI / ProviderRequest | [DESIGN.md §5](./DESIGN.md#5-provideradapter-spi--providerrequest-中间层) |
| SSE 解析 / SseFrame | [DESIGN.md §5.1](./DESIGN.md#51-sse-解析--sseframe-中间表示) |
| 流式传输层解耦 | [DESIGN.md §5.2](./DESIGN.md#52-流式处理--传输层解耦eventsink--streamevent) |
| ToolError 类型化 + RetryPolicy 集成 | [DESIGN.md §7.1-7.2](./DESIGN.md#7-%E5%87%86%E5%A4%8D%E5%B1%82---retypolicy%E5%8B%BE%E6%97%B6%E6%95%85%E9%9A%9C%E4%B8%8E-fallbackstrategy%E8%B7%AF%E7%94%B1%E5%86%B3%E7%AD%96) |
| FallbackStrategy 路由决策 | [DESIGN.md §7.3](./DESIGN.md#73-fallbackstrategy---%E8%B7%AF%E7%94%B1%E5%86%B3%E7%AD%96%EF%BC%88%22%E6%8D%A2%E6%9D%A1%E8%B7%AF%E8%B5%B0%22%EF%BC%89) |
| Message 语义校验 | [DESIGN.md §9](./DESIGN.md#9-message-%E8%AF%AD%E4%B9%89%E6%A0%A1%E9%AA%8C) |
| build_request 序列化完整性 | [DESIGN.md §10](./DESIGN.md#10-build_request-%E5%BA%8F%E5%88%97%E5%8C%96%E5%AE%8C%E6%95%B4%E6%80%A7) |

## 六、实现状态

### v0.1 闭环

```
ChatRequest → LLM(Provider) → ToolCall → ToolExecution → ToolResult → LLM (循环)
```

### 核心模块

| 模块 | 状态 |
|------|------|
| lellm-core 协议对象 | ✅ |
| LlmProvider trait | ✅ |
| ProviderAdapter SPI | ✅ |
| GenericProvider | ✅ |
| stream/ 传输层解耦 | ✅ |
| SseParser + ToolCallAccumulator | ✅ |
| EventSink + StreamEvent | ✅ |
| ToolExecutor | ✅ |
| ToolUseLoop | ✅ |
| ModelRouter + Registry | ✅ |
| ShortTermMemory | ✅ |
| ToolRegistry | ✅ |

### Resilience Layer

| 模块 | 类型定义 | 集成状态 | v0.1 范围 |
|------|---------|---------|----------|
| ToolError (类型化) | ✅ | ✅ | ✅ 必须 |
| RetryPolicy → ToolExecutor | ✅ | ✅ | ✅ 必须 |
| FallbackStrategy → ToolUseLoop | ✅ | ✅ | ✅ 必须 |
| Retry AgentEvent | ✅ | — | ⚠️ v0.2 |
| LoopDetector | ✅ | 🔒 `v02-preview` | ❌ v0.2 |
| SignalVoter | ✅ | 🔒 `v02-preview` | ❌ v0.2 |

> **v0.1 铁律：** 仓库中不允许存在 "Runtime 永远不会调用到的恢复模块"。要么接入，要么标记 v0.2。

### 待完成

| 优先级 | 模块 | 状态 |
|--------|------|------|
| P0 | `ToolError` 类型化 + `ToolResult = Result<String, ToolError>` | ✅ 已完成（core/error.rs） |
| P0 | `ToolExecutor` 集成 `RetryPolicy` | ✅ 已完成（agent/tools/executor.rs） |
| P1-H | AnthropicAdapter `parse_response` / `parse_sse_frame` 完善 | ✅ 已完成 |
| P1-H | OpenAICompatAdapter `build_request` 完善 | ✅ 已完成 |
| P3 | lellm-macros derive | 🟡 Stub |
| P4 | examples/ | 🟡 部分 |

## 七、版本路线图

| 版本 | 范围 |
|------|------|
| **v0.1** | core + provider + agent + macros |
| **v0.2** | Graph/Node/Edge + LoopDetector/SignalVoter 集成 |
| **v0.3** | MCP Client/Server + Multi-Agent |
