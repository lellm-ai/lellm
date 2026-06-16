# lellm v0.1 产品蓝图

> 版本：v0.1 | 日期：2026-06-15 | 状态：代码已对齐
> 设计决策详见 [DESIGN.md](./DESIGN.md)

## 一、项目愿景

做 Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，标准化消息内容格式；提供基础的 LLM provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use, MCP client
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration（v0.2+）
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 二、v0.1 范围

### 包含

| Crate | 职责 | 核心内容 |
|-------|------|----------|
| `lellm` | 门面 crate | Feature-gated re-export 所有子 crate；用户统一入口 |
| `lellm-core` | 协议对象 | Message, ContentBlock, ChatRequest/Response, ToolDefinition, CacheControl, TokenUsage, LlmError |
| `lellm-provider` | Provider trait + Codec | LlmProvider, CodecProvider, ProviderExtension (三权分立), ModelRouter, ProviderRegistry, MockProvider |
| `lellm-agent` | Agent 运行时 | ToolExecutor, ToolUseLoop, AgentEvent, ParallelSafety, RetryPolicy, FallbackStrategy |
| `lellm-macros` | 派生宏 + 属性宏 | `#[tool]` 函数宏, `#[derive(Tool)]` struct 宏, `ToolDefinition` 向后兼容别名 |
| `lellm-mcp` | MCP Client | McpClient, McpTransport (stdio), McpCatalog (ToolCatalog), ToolBridge |

### 不包含（v0.2+）

- Graph/Node/Edge 编排层
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
├── lellm-macros/               # 派生宏
└── lellm-mcp/                  # MCP Client（v0.1 提前纳入）
```

> 完整文档：[MCP 集成设计](./mcp-design.md)

## 四、架构总览

```
用户
 ↓
Graph (v0.2)
 ↓
Agent (ToolUseLoop)
 ↓
ToolExecutor
 ↓
ToolRegistration
 ├─ LocalTool (现有)
 └─ McpToolBridge (v0.1)
      ↓
   McpClient
      ↓
   McpTransport (stdio)
      ↓
   外部 MCP Server
```

## 五、核心 API

### 4.1 LlmProvider

```rust
pub trait LlmProvider: Send + Sync {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn stream(
        &self,
        request: &ChatRequest,
    ) -> Result<ProviderStream, LlmError>;
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
       → CodecProvider<C> (框架内部)
          → ProviderExtension 三权分立 (生态扩展 SPI)
              ├── ChatCodec (协议编解码)
              ├── ModelCapabilities (能力矩阵)
              └── ProviderMeta (连接元数据)
```

详见 [DESIGN.md §5](./DESIGN.md#5-providercodec---%E4%B8%89%E6%9D%83%E5%88%86%E7%AB%8B%E7%9A%84%E5%8D%8F%E8%AE%AE%E7%BC%96%E8%A7%A3%E7%A0%81-spi)

## 五、关键设计决策索引

| 主题 | 详见 |
|------|------|
| 门面 crate 与 Feature Gate | [DESIGN.md §0](./DESIGN.md#0-lellm-门面-crate) / [§16](./DESIGN.md#16-门面-crate-feature-gate) |
| MaxIterationsReached 归 Ok | [DESIGN.md §1](./DESIGN.md#1-maxiterationsreached--ok-还是-err) |
| AgentEvent 终态契约 | [DESIGN.md §2](./DESIGN.md#2-agentevent-终态契约) |
| ToolUseLoop 与 model 单向流动 | [DESIGN.md §3](./DESIGN.md#3-tooluseloop-不知道-router--registry--model-单向流动) |
| ProviderCodec 三权分立 | [DESIGN.md §5](./DESIGN.md#5-providercodec---三权分立的协议编解码-spi) |
| ToolError 类型化 + RetryPolicy 集成 | [DESIGN.md §7.1-7.2](./DESIGN.md#7-%E5%87%86%E5%A4%8D%E5%B1%82---retypolicy%E5%8B%BE%E6%97%B6%E6%95%85%E9%9A%9C%E4%B8%8E-fallbackstrategy%E8%B7%AF%E7%94%B1%E5%86%B3%E7%AD%96) |
| FallbackStrategy 路由决策 | [DESIGN.md §7.3](./DESIGN.md#73-fallbackstrategy---%E8%B7%AF%E7%94%B1%E5%86%B3%E7%AD%96%EF%BC%88%22%E6%8D%A2%E6%9D%A1%E8%B7%AF%E8%B5%B0%22%EF%BC%89) |
| AgentEvent 流式阶段事件 | [DESIGN.md §8](./DESIGN.md#8-agentevent-流式阶段事件) |
| Message 语义校验 | [DESIGN.md §9](./DESIGN.md#9-message-%E8%AF%AD%E4%B9%89%E6%A0%A1%E9%AA%8C) |
| build_request 序列化完整性 | [DESIGN.md §10](./DESIGN.md#10-build_request-%E5%BA%8F%E5%88%97%E5%8C%96%E5%AE%8C%E6%95%B4%E6%80%A7) |
| 输出 + 推理预算保险丝 | [DESIGN.md §11](./DESIGN.md#11-%E8%BE%93%E5%87%BA%E9%A4%88%E7%AE%97%E4%BF%9D%E9%99%A9%E4%B8%9D) |
| 日志降噪 | [DESIGN.md §12](./DESIGN.md#12-%E6%97%A5%E5%BF%97%E5%87%8F%E5%99%B2) |
| 推理控制 | [DESIGN.md §13](./DESIGN.md#13-%E6%8E%A8%E7%90%86%E6%8E%A7%E5%88%B6--reasoningconfig-stream_thinking-%E4%B8%A4%E5%B1%82%E6%A8%A1%E5%9E%8B) |
| RequestOptions | [DESIGN.md §14](./DESIGN.md#14-requestoptions--agent-%E5%B1%8F%E7%94%9F%E6%88%90%E5%8F%82%E6%95%B0%E8%A6%BD%E7%9B%96) |
| Context Compaction | [DESIGN.md §15](./DESIGN.md#15-context-compaction---%E4%B8%8A%E4%B8%8B%E6%96%87%E5%8E%8B%E7%BC%A9) |

### MCP 设计决策（v0.3+）

详见 [MCP 集成设计](./mcp-design.md)。

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
| ProviderCodec 三权分立 | ✅ |
| CodecProvider | ✅ |
| ProviderBuilder + CodecProvider::openrouter() + ProviderProfile | ✅ |
| ProviderMeta::default_headers() | ✅ |
| stream/ 传输层解耦 | ✅ |
| SseParser + ToolCallAccumulator | ✅ |
| EventSink + StreamEvent | ✅ |
| ToolExecutor | ✅ |
| ToolUseLoop | ✅ |
| AgentBuilder | ✅ |
| ModelRouter + Registry | ✅ |
| ShortTermMemory | ❌ v0.1 删除（LoopState 足够） |
| ToolRegistry | ❌ v0.1 删除（ToolExecutor 内置 HashMap） |
| 输出预算保险丝 | ✅ |
| 推理预算保险丝 | ✅ |
| Context Compaction | ✅ |
| RequestOptions | ✅ |
| derive(ToolDefinition) | ✅ |

### 输出预算

| 功能 | 状态 | 说明 |
|------|------|------|
| `max_output_tokens` 单轮上限 | ✅ | 注入 `ChatRequest.max_tokens`，流式消费时实时检查 |
| `max_total_output_tokens` 总上限 | ✅ | 整个 Agent Run 累计，防止多轮成本失控 |
| `StopReason::OutputBudgetExceeded` | ✅ | 区分 token 超限与轮次超限 |
| `estimate_text()` 导出 | ✅ | CJK-aware 启发式，供流式 delta 增量估算 |
| 日志降噪 | ✅ | 移除 stream_processor 高频 trace，channel 满静默丢弃 |

### 推理预算

| 功能 | 状态 | 说明 |
|------|------|------|
| `ChatRequest.max_reasoning_tokens` 单轮上限 | ✅ | 流式消费 ThinkingDelta 时实时检查 |
| `StopReason::ReasoningBudgetExceeded` | ✅ | 区分推理超限与输出超限 |
| `LoopState.total_reasoning_tokens` | ✅ | 累计推理 Token |
| `max_total_reasoning_tokens` 总上限 | ✅ | 非流式 + 流式均已接入 |

### Context Compaction

| 功能 | 状态 | 说明 |
|------|------|------|
| ContextBudget 配置 | ✅ | max_tokens / warning_ratio / keep_recent_turns |
| ContextCompactor trait | ✅ | 可插拔压缩策略 |
| LocalCompactor | ✅ | 滑动窗口 + 纯文本摘要 |
| ContextCompacted 事件 | ✅ | 可观测性 |
| 工具结果截断 | ✅ | 两条路径统一截断 |

### Resilience Layer

| 模块 | 类型定义 | 集成状态 | v0.1 范围 |
|------|---------|---------|----------|
| ToolError (类型化) | ✅ | ✅ | ✅ 必须 |
| RetryPolicy → ToolExecutor | ✅ | ✅ | ✅ 必须 |
| FallbackStrategy → ToolUseLoop | ✅ | ✅ | ✅ 必须 |
| AgentEvent::Retry | ✅ | ✅ | ✅ 已接入 |
| LoopDetector | ✅ | 🔒 `v02-preview` | ❌ v0.2 |
| SignalVoter | ✅ | 🔒 `v02-preview` | ❌ v0.2 |

> **v0.1 铁律：** 仓库中不允许存在 "Runtime 永远不会调用到的恢复模块"。要么接入，要么标记 v0.2。

### 待完成

| 优先级 | 模块 | 状态 |
|--------|------|------|
| P0 | `ToolError` 类型化 + `ToolResult = Result<String, ToolError>` | ✅ 已完成（core/error.rs） |
| P0 | `ToolExecutor` 集成 `RetryPolicy` | ✅ 已完成（agent/tools/executor.rs） |
| P1-H | AnthropicCodec `encode` / `decode_sse` 完善 | ✅ 已完成 |
| P1-H | OpenAICompatCodec `encode` 完善 | ✅ 已完成 |
| P3 | lellm-macros derive | ✅ 已完成 |
| P4 | examples/ | ✅ 已完成 |

## 七、版本路线图

| 版本 | 范围 |
|------|------|
| **v0.1** | core + provider + agent + macros + MCP (Tools only) |
| **v0.2** | Graph/Node/Edge + 有环图 + BarrierNode + 流式执行 + 错误二分法 |
| **v0.3** | ParallelNode + Checkpoint + 持久化 |
| **v0.4** | Multi-Agent Orchestration + MCP Server + Resources |
| **v0.5** | Sampling + Agent↔Agent via MCP |
