# lellm v0.1 产品蓝图

> 版本：v0.1 | 日期：2026-06-02 | 状态：代码已对齐

## 一、项目愿景

做 Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，以及帮助快速构建常用应用的高层接口；标准化消息内容格式；提供基础的 LLM provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use, mcp client/server
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 二、v0.1 范围

### 包含

| Crate | 职责 | 核心内容 |
|-------|------|----------|
| `lellm-core` | 协议对象 | Message, ContentBlock(Text/Thinking/Image/ToolCall), ChatRequest/Response, ToolDefinition, TokenUsage, LlmError, ToolError, MemoryError, ParseError, LellmError |
| `lellm-provider` | Provider trait + 适配器 | LlmProvider trait (call/stream), ProviderEvent, ProviderAdapter (pub(crate)), GenericProvider<A>, ModelRouter, ProviderRegistry, ResolvedModel, MockProvider |
| `lellm-agent` | Agent 运行时 | ToolExecutor, ToolUseLoop, AgentEvent, StopReason, ParallelSafety, ToolCategory, ToolRegistry, RetryPolicy, LoopDetector, FallbackStrategy, ShortTermMemory, SignalVoter |
| `lellm-macros` | 派生宏 | `#[derive(ToolDefinition)]` — Stub，待实现 JSON Schema 生成 + 参数反序列化 |

### 不包含（v0.2+）

- Graph/Node/Edge 编排层（v0.1 完全不包含）
- MCP Client/Server（v0.1 仅预留 `ToolSource::Mcp`）
- Sandbox（v0.1 暂不提取）
- Harness Orchestrator（v0.1 暂不提取）
- LongTermMemory / MemoryStore（v0.1 暂不实现）

## 三、Workspace 结构

```
lellm/
├── Cargo.toml                  # workspace root, edition 2024
├── docs/
│   └── BLUEPRINT.md            # 本文件
├── lellm-core/                 # 协议对象，零运行时依赖
│   ├── Cargo.toml              # deps: serde, serde_json, thiserror
│   └── src/
│       ├── lib.rs              # re-exports
│       ├── message.rs          # Message, ContentBlock, TextBlock, ThinkingBlock, ImageSource, ToolCall, text_block()
│       ├── request.rs          # ChatRequest, ToolDefinition, ToolChoice
│       ├── response.rs         # ChatResponse, TokenUsage
│       └── error.rs            # LellmError, LlmError, ToolError, MemoryError, ParseError
├── lellm-provider/             # Provider trait + 适配器
│   ├── Cargo.toml              # features: openai, anthropic, openai-compat, mock
│   ├── examples/               # 使用示例
│   │   ├── quickstart.rs       # 最简 Provider 调用 (⚠️ Stub)
│   │   ├── conversation.rs     # 多轮对话 (⚠️ Stub)
│   │   ├── streaming.rs        # 流式调用 (⚠️ Stub)
│   │   ├── router_registry.rs  # ModelRouter + ProviderRegistry (⚠️ Stub)
│   │   └── mock_test.rs        # MockProvider 测试 ✅ 可运行
│   └── src/
│       ├── lib.rs              # LlmProvider trait, ProviderEvent, ProviderStream
│       ├── router.rs           # ModelRouter, ProviderRegistry, ResolvedModel, RouteEntry, TaskLevel
│       └── providers/
│           ├── mod.rs
│           ├── base.rs         # GenericProvider<A>, ProviderAdapter trait, HttpRequest, HttpResponse, StreamChunk, StreamParseResult, ToolCallAccumulator, ProviderConfig
│           ├── anthropic.rs    # AnthropicAdapter (Stub, feature = "anthropic")
│           ├── openai_compat.rs # OpenAI兼容适配器 (Stub, 覆盖 OpenAI/NVIDIA/DeepSeek/VLLM/LLaMA)
│           └── mock.rs         # MockProvider (feature = "mock", 测试用)
├── lellm-agent/                # Agent 运行时
│   ├── Cargo.toml              # deps: lellm-core, lellm-provider
│   └── src/
│       ├── lib.rs              # re-exports
│       ├── tools/
│       │   ├── mod.rs          # AgentEvent, StopReason, AgentStream
│       │   ├── registry.rs     # ToolRegistry, ToolSource, ToolSearchResult
│       │   ├── executor.rs     # ToolExecutor, ParallelSafety, ToolCategory, ToolRegistration
│       │   ├── loop_.rs        # ToolUseLoop, ToolUseResult, ToolCallResult
│       │   ├── retry.rs        # ToolErrorKind, RetryPolicy, BackoffStrategy
│       │   ├── loop_detector.rs # LoopDetector, ToolCallFingerprint, LoopIntervention
│       │   ├── fallback.rs     # FallbackStrategy trait, FallbackReason, FallbackContext, FallbackAction, DefaultFallback
│       │   └── signal_voter.rs # SignalVoter, NegativeSignal
│       └── memory/
│           ├── mod.rs
│           └── short_term.rs   # ShortTermMemory (VecDeque 环形缓冲, 默认 200 容量)
└── lellm-macros/               # 派生宏
    ├── Cargo.toml              # proc-macro = true, deps: proc-macro2, quote, syn
    └── src/
        └── lib.rs              # #[derive(ToolDefinition)] — Stub
```

## 四、核心设计决策

### 4.1 Crate 划分原则

**Crate 拆分准则：** 只有当某个模块未来可能被单独引用（被其他项目依赖）时，才值得拆成独立 crate。

**Core 原则：**
- 零 Provider 依赖
- 零 Runtime 依赖（仅 serde + serde_json + thiserror）
- 尽量少 Feature
- 能被所有 crate 引用

**v0.1 选择 4 个 crate 而非 5 个的原因：**

1. **Memory 不单独 crate** — ShortTermMemory 本质是 `VecDeque<Message>`。与 Agent Loop 高度耦合，很少被单独引用。
2. **Tool System 不单独 crate** — ToolRegistry、ToolExecutor、ToolUseLoop、Fallback、LoopDetector 全部是 Agent Runtime 的组成部分。LangChain、AutoGen、OpenAI Agents SDK 都没有将 Tool 独立成包。
3. **真正独立的是协议对象** — ToolDefinition、ToolCall、Message 等放在 `lellm-core`，作为统一抽象层（类似 openai-types / anthropic-types 的统一版）。

### 4.2 发布策略 — 分阶段

v0.1 只确保 **LLM ↔ Tool Call** 核心闭环独立可运行且通过测试。Graph 编排和 MCP 建立在此闭环之上。

### 4.3 Feature Gate

provider 适配器通过 feature flag 可选编译，用户只编译需要的 provider。避免拆成独立 crate 的过度设计。

```toml
[features]
default = ["openai", "anthropic", "openai-compat"]
openai = []
anthropic = []
openai-compat = []
mock = []        # 测试用 MockProvider
```

### 4.4 剥离策略 — 复制 + 净化

1. 读取 devops-agent 源文件
2. 手动移除所有对 devops-agent 内部模块的引用
3. 用 trait/接口替代具体类型依赖
4. 写入 lellm workspace，编写独立测试
5. 最后让 devops-agent `Cargo.toml` 依赖 `lellm-*`，删除原代码

### 4.5 流式支持 — 分层事件流

**Provider 层事件：**

```rust
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, LlmError>> + Send>>;

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    Start { model: String },
    Token { token: String },
    Done { tool_calls: Vec<ToolCall>, usage: Option<TokenUsage> },
}
```

**Agent 层事件：**

```rust
pub type AgentStream = tokio::sync::mpsc::Receiver<AgentEvent>;

#[derive(Debug)]
pub enum AgentEvent {
    Provider(ProviderEvent),           // passthrough from provider
    ToolStart { tool_call_id, name },
    ToolEnd { tool_call_id, result },
    Retry { tool_call_id, attempt, max_attempts, reason },
    LoopEnd { result: ToolUseResult }, // 终态 — 正常结束, 恰好一次
    LoopError { error, iterations, messages }, // 终态 — 异常结束, 恰好一次
}
```

**终态契约：**
- 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
- 异常结束：`LoopError` 恰好一次，然后 channel 关闭
- 终态事件后不再发送任何事件
- `MaxIterationsReached` 视为 Agent 层正常终止（返回 `Ok(ToolUseResult)`），非 Provider 错误

ToolUseLoop 提供两个接口：

```rust
impl ToolUseLoop {
    // 非流式 — Ok 含 MaxIterationsReached, Err 仅为 Provider 调用失败
    pub async fn execute(self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError>;

    // 流式 — 返回事件接收器
    pub fn execute_stream(self, messages: Vec<Message>) -> AgentStream;
}
```

内部累积 + 外部流式：纯文本部分实时发送 `Token`，tool_calls 在 LLM 返回完成后统一解析并原子执行。

### 4.5.5 ContentBlock 统一协议

`ContentBlock` 是 Message 和 ChatResponse 的唯一内容载体：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    Image { source: ImageSource },
    ToolCall(ToolCall),
}
```

**ToolResult 不在 ContentBlock 中** — `Message::ToolResult` 是独立的 Message 变体，携带 `tool_call_id` + `content: Vec<ContentBlock>`。

**ChatResponse 与 Message::Assistant 对齐：**

```rust
pub struct ChatResponse {
    pub content: Vec<ContentBlock>,   // 与 Message::Assistant 一致
    pub tool_calls: Vec<ToolCall>,    // 冗余缓存，从 content 中自动提取
    pub usage: TokenUsage,
    pub raw: serde_json::Value,       // provider 特有字段兜底
}
```

`ChatResponse::new(content, usage, raw)` 构造函数自动从 content 中提取 tool_calls 到冗余缓存，方便访问。

### 4.6 工具执行 — 按 ParallelSafety 分级

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelSafety {
    Safe,                  // 可安全并行
    CategoryExclusive,     // 同类互斥（类别从 ToolCategory 读取）
    Exclusive,             // 完全互斥
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolCategory(pub Cow<'static, str>);

impl ToolCategory {
    pub const FILE_IO: Self = Self(Cow::Borrowed("file_io"));
    pub const NETWORK: Self = Self(Cow::Borrowed("network"));
    pub const DATABASE: Self = Self(Cow::Borrowed("database"));
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self;
}
```

**ToolRegistration — 工具注册信息：**

```rust
impl ToolRegistration {
    pub fn safe(f) -> Self;                        // 可并行
    pub fn category_exclusive(category, f) -> Self; // 同类互斥
    pub fn exclusive(f) -> Self;                   // 完全互斥
}
```

**ToolExecutor — 按名称分派：**

```rust
impl ToolExecutor {
    pub fn new() -> Self;
    pub fn register(&mut self, name: &str, reg: ToolRegistration);
    pub async fn execute(&self, call: &ToolCall) -> ToolCallResult;
    pub async fn execute_batch(&self, calls: &[ToolCall]) -> Vec<Message>;
    pub fn partition_calls(&self, calls: &[ToolCall]) -> (Vec<ToolCall>, Vec<ToolCall>);
}
```

执行器逻辑：
- `Safe` → 可 `tokio::join!` 并行（partition_calls 分组）
- `CategoryExclusive` → 按 category 分组，组内串行、组间并行
- `Exclusive` → 全局逐个串行

### 4.7 Provider 架构 — 三层分离

```
用户          LlmProvider (public API)
               ↓
框架内部      GenericProvider<A>
               ↓
内部 SPI      ProviderAdapter (pub(crate))
```

```rust
// ─── 对外 Public API ───
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError>;
    fn provider_id(&self) -> &str;
}

// ─── 内部 SPI (pub(crate)) ───
pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn build_request(&self, req: &ChatRequest) -> Result<HttpRequest, LlmError>;
    fn parse_response(&self, resp: &HttpResponse) -> Result<ChatResponse, LlmError>;
    fn parse_stream_chunk(&self, chunk: &[u8]) -> Result<StreamParseResult, LlmError>;
}

// ─── GenericProvider<A> — 封装通用逻辑（重试、超时、流式解析）
// 注意：当前未 impl LlmProvider，仅作为 Adapter 容器
```

`OpenAICompatAdapter` 一个实现覆盖 7+ provider（OpenAI, NVIDIA, DeepSeek, VLLM, LLaMA, LM Studio, Ollama, OpenRouter），仅 `base_url` 不同。

提供工厂方法：`OpenAICompatAdapter::openai()`, `::nvidia()`, `::deepseek()`, `::vllm()`, `::llama()`。

### 4.8 ModelRouter — 三级路由（配置解析器）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskLevel { Flash, Standard, Pro }

#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub provider_id: String,
    pub model: String,
}

pub struct ModelRouter {
    routes: HashMap<TaskLevel, RouteEntry>,
}

impl ModelRouter {
    pub fn new() -> Self;
    pub fn add_route(&mut self, level: TaskLevel, entry: RouteEntry);
    pub fn resolve(&self, level: TaskLevel) -> Option<&RouteEntry>;
}
```

**Router 不持有 `Arc<dyn LlmProvider>`** — 只做配置解析，返回 `RouteEntry` 由外部组装。

### 4.8.1 ProviderRegistry — Provider 容器

```rust
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, id: &str, provider: Arc<dyn LlmProvider>);
    pub fn get(&self, id: &str) -> Option<Arc<dyn LlmProvider>>;
    pub fn resolve(&self, route: &RouteEntry) -> Result<ResolvedModel, LlmError>;
}
```

**ResolvedModel — 解析后的模型绑定：**

```rust
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn LlmProvider>,
    pub model: String,
}
```

### 4.8.2 使用模式

```rust
let route = router.resolve(TaskLevel::Flash)?;
let resolved = registry.resolve(route)?;  // -> ResolvedModel
resolved.provider.call(request.with_model(resolved.model.clone()));
```

或直接在 Agent 层使用：

```rust
let loop_ = ToolUseLoop::new(resolved_model, tool_executor)
    .set_max_iterations(10);
let result = loop_.execute(messages).await?;
```

### 4.9 配置管理

```rust
let config = ProviderConfig {
    base_url: "https://api.openai.com".into(),
    api_key: "sk-...".into(),
    model: "gpt-4".into(),
    timeout_secs: 120,
};
let provider = GenericProvider::new(adapter, config);
```

库不读配置文件，配置加载留给上层应用。

### 4.10 Tool 声明 — Schema 与 Runtime 分离

**核心原则：** Tool Schema（给 LLM 看）与 Tool Runtime（给 Agent 执行）是完全不同的职责，不应绑死。

```rust
// ─── Schema 层 — derive 宏生成 JSON Schema + 反序列化 (Stub, 待实现) ───
#[derive(ToolDefinition)]
pub struct ReadFileArgs {
    /// 文件路径
    pub path: String,
}

// 宏将生成：
// impl ReadFileArgs {
//     fn schema() -> ToolDefinition { ... }
//     fn from_args(value: &serde_json::Value) -> Result<Self, ToolError> { ... }
// }

// ─── Runtime 层 — 用户注册工具函数 ───
let mut executor = ToolExecutor::new();
executor.register("read_file", ToolRegistration::category_exclusive(
    ToolCategory::FILE_IO,
    |args: &serde_json::Value| async {
        let path = args.get("path").unwrap().as_str().unwrap();
        // ... 执行文件读取
        ToolCallResult::Ok(contents)
    },
));
```

**ToolRegistry — 工具搜索：**

```rust
pub enum ToolSource { Builtin, Dynamic, Mcp, Skill }

impl ToolRegistry {
    pub fn register(&mut self, name: &str, source: ToolSource, def: ToolDefinition);
    pub fn add_synonyms(&mut self, tool_name: &str, synonyms: &[&str]);
    pub fn search(&self, query: &str) -> Vec<ToolSearchResult>;    // 精确 → 同义词 → 子串兜底
    pub fn search_category(&self, category: &str) -> Vec<ToolSearchResult>;
    pub fn list_tools(&self) -> Vec<ToolSearchResult>;
}
```

**为什么这样设计：**
- Local Tool、MCP Tool、Remote Tool、HTTP Tool 未来都可接入同一 Registry
- Schema 与 Runtime 解耦，避免框架把三合一绑死导致的重构痛苦

### 4.11 循环检测 — 指纹去重 + 阈值触发

```rust
pub struct LoopDetector {
    history: Vec<ToolCallFingerprint>,
    threshold: usize,
}

pub struct ToolCallFingerprint {
    pub tool_name: String,
    pub normalized_args: String,  // JSON 键排序 + 空白去除
}

pub enum LoopIntervention {
    InjectHint(String),  // 注入系统提示干预
    Break,               // 中断循环
}
```

相同指纹连续出现超过阈值 → 返回 `LoopIntervention::InjectHint`。

### 4.12 Fallback 策略 — Agent Runtime 统一恢复机制

**归属：** `lellm-agent`（不是 Provider 层，不是 Graph 层）

```rust
pub enum FallbackReason {
    LlmError(LlmError),
    ToolError(ToolError),
    LoopDetected,
    MaxIterationsReached,
}

pub struct FallbackContext {
    pub reason: FallbackReason,
    pub conversation: Arc<[Message]>,
    pub attempt: usize,
    pub max_attempts: usize,
}

#[derive(Debug, Clone)]
pub enum FallbackAction {
    Retry,                        // 原样重试
    RetryWithMessages(Vec<Message>), // 注入消息后重试
    SwitchProvider(String),        // 切换 provider
    Complete(ChatResponse),        // 直接返回响应
    Abort,                        // 放弃
}

#[async_trait]
pub trait FallbackStrategy: Send + Sync {
    async fn handle(&self, ctx: &FallbackContext) -> FallbackAction;
}
```

**DefaultFallback：** 对 Timeout/Network/5xx 错误重试（默认 3 次），其余直接 Abort。

### 4.13 负信号投票

```rust
pub enum NegativeSignal {
    ToolError,
    ToolTimeout,
    RepeatedToolCall,
    LowConfidence,
}

pub struct SignalVoter {
    signals: Vec<NegativeSignal>,
    threshold: usize,  // 默认 5
}
```

累积负信号，达到阈值时触发升级（返回 true）。

### 4.14 重试策略

```rust
pub enum ToolErrorKind {
    Timeout,        // 可重试, max=5, 指数退避
    PermissionDenied, // 不可重试
    NotFound,         // 不可重试
    NetworkError,     // 可重试, max=3, 固定 3s
    ParseError,       // 不可重试
    Unknown,          // 可重试, max=3, 固定 3s
}

pub enum BackoffStrategy {
    Fixed(Duration),
    Exponential { base: Duration, max: Duration },
}

impl RetryPolicy {
    pub async fn execute_with_retry<F, Fut>(kind: ToolErrorKind, f: F) -> ToolCallResult;
}
```

每种错误类型自带 `hint()` 提示语，用于注入上下文引导 LLM 调整策略。

### 4.15 ContentBlock — 核心层极简

ContentBlock 仅 4 个变体：Text、Thinking、Image、ToolCall。

- **无 `ToolResult`** — ToolResult 使用 `Message::ToolResult` 变体
- **无 `cache_control`** — provider 特有标记下沉到 adapter 层
- **`serde(tag = "type")`** — 统一 tagged enum 序列化

### 4.16 错误体系 — 每层自定义 Error

```rust
// lellm-core — 顶层
pub enum LellmError {
    Llm(LlmError),
    Tool(ToolError),
    Memory(MemoryError),
    Parse(ParseError),
}

// LLM API 错误
pub enum LlmError {
    ApiError { provider, status, code, message },
    Timeout,
    ParseError { detail },
    Network { detail },
    ModelNotFound { model },
    Other { message },
}

// 工具执行错误
pub enum ToolError { NotFound, ExecutionFailed, Timeout, LoopDetected }

// 记忆操作错误
pub enum MemoryError { IoError, DatabaseError }

// 解析错误
pub struct ParseError { detail: String }
```

所有错误均实现 `thiserror`，子类型通过 `#[from]` 自动转 LellmError。

### 4.17 异步运行时 — 分层绑定

| Crate | 运行时绑定 |
|-------|-----------|
| `lellm-core` | 零运行时（仅 serde + serde_json + thiserror） |
| `lellm-provider` | tokio + reqwest |
| `lellm-agent` | tokio + futures |
| `lellm-macros` | proc-macro，零运行时 |

### 4.18 测试策略 — 三层 + Mock Provider

| 层级 | 内容 |
|------|------|
| 单元测试 | 每个 crate 内部 `#[cfg(test)]` 模块 |
| 集成测试 | `tests/` 目录，跨 crate 集成场景 |
| 示例项目 | `examples/` 可运行 demo（待补充） |

MockProvider 通过 `feature = "mock"` 提供，支持预设响应和 tool_call 注入，实现完整 `LlmProvider` trait。

## 五、实现状态

### v0.1 已完成

| 优先级 | 模块 | 状态 | 说明 |
|--------|------|------|------|
| P1 | lellm-core 协议对象 | ✅ 完成 | Message, ContentBlock, ChatRequest/Response, 错误体系 |
| P2 | LlmProvider trait | ✅ 完成 | call/stream/provider_id, ProviderEvent, ProviderStream |
| P3 | ProviderAdapter SPI | ✅ 完成 | GenericProvider<A>, Adapter trait, StreamParseResult |
| P4 | ToolExecutor | ✅ 完成 | register/execute/execute_batch/partition_calls |
| P5 | ToolUseLoop | ✅ 完成 | execute + execute_stream, AgentEvent 终态契约 |
| P6 | ModelRouter + Registry | ✅ 完成 | TaskLevel 路由, ProviderRegistry.resolve() |
| P7 | Fallback + Retry | ✅ 完成 | FallbackStrategy, DefaultFallback, RetryPolicy |
| P8 | ShortTermMemory | ✅ 完成 | VecDeque 环形缓冲, 默认 200 容量 |
| - | LoopDetector | ✅ 完成 | 指纹归一化 + 阈值检测 |
| - | SignalVoter | ✅ 完成 | 负信号累积 + 阈值触发 |
| - | ToolRegistry | ✅ 完成 | 精确/同义词/子串搜索, 分类搜索 |

### v0.1 待完成

| 优先级 | 模块 | 状态 | 说明 |
|--------|------|------|------|
| P1-H | AnthropicAdapter | 🔴 Stub | build_request/parse_response/parse_stream_chunk 均未实现 |
| P1-H | OpenAICompatAdapter | 🔴 Stub | 同上 |
| P2 | GenericProvider impl LlmProvider | 🟡 缺失 | 当前仅有 new() 构造器，未实现 call/stream |
| P3 | lellm-macros derive | 🟡 Stub | 解析输入但返回空 TokenStream |
| P4 | examples/ | 🟡 部分 | 5 个 example 已创建，mock_test 可运行；其余待 Adapter 实现 |
| P5 | 集成测试 | 🟡 缺失 | 跨 crate E2E 测试 |

## 六、版本路线图

| 版本 | 范围 | 状态 |
|------|------|------|
| **v0.1** | core + provider + agent + macros | 核心闭环已完成 |
| **v0.2** | Graph/Node/Edge 编排层 + MCP Client | 规划中 |
| **v0.3** | MCP Server + Multi-Agent Orchestration | 规划中 |

### 未来 Crate 结构

```
lellm-core        # 协议对象（不变）
    ↓
lellm-provider    # Provider 适配（不变）
    ↓
lellm-agent       # Agent 运行时（不变）
    ↓
lellm-graph       # v0.2 — Graph/Node/Edge 编排
    ↓
lellm-mcp         # v0.3 — MCP Client/Server
```

依赖关系：`core → provider → agent → graph`，清晰分层。

### v0.2 — lellm-graph 规划

v0.2 将创建独立的 `lellm-graph` crate，实现完整的 Graph 编排能力（类似 LangGraph）。

**核心设计原则：**
- Node、Edge、GraphState 等属于 **Graph Runtime**，不属于 Core Protocol
- v0.1 的 Core 已彻底剥离 graph.rs，Core 只做协议对象
- v0.2 再决定 Node trait 用 `async-trait` 还是 `impl Future`

**lellm-graph 目录结构：**

```
lellm-graph/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── node.rs       # Node trait — 接收状态并返回新状态
    ├── edge.rs       # Edge — 条件边、默认边
    ├── graph.rs      # Graph, GraphState — 节点间共享的键值存储
    ├── executor.rs   # GraphExecutor — 图的执行调度器
    ├── checkpoint.rs # CheckpointStore — 执行状态持久化
    ├── interrupt.rs  # HumanInterrupt — 人类介入点
    ├── subgraph.rs   # SubGraph — 子图嵌套
    └── multi_agent.rs # MultiAgentGraph — 多 Agent 编排
```

**Node trait 设计方向（v0.2 时再拍板）：**

```rust
// 方案 A：async-trait（简洁但间接调用开销）
#[async_trait]
pub trait Node: Send + Sync {
    fn name(&self) -> &str;
    async fn execute(&self, state: &mut GraphState) -> Result<NodeResult, GraphError>;
}

// 方案 B：返回 Future（零间接调用开销但签名复杂）
pub trait Node: Send + Sync {
    fn name(&self) -> &str;
    fn execute(
        &self,
        state: &mut GraphState,
    ) -> impl Future<Output = Result<NodeResult, GraphError>> + Send;
}
```

**GraphState 设计方向：**

```rust
// 基于 serde_json::Value 的灵活方案
pub type GraphState = HashMap<String, serde_json::Value>;

// 或基于强类型的方案（v0.2 时根据实际需求决定）
pub struct GraphState {
    data: HashMap<String, serde_json::Value>,
    version: u64,       // 乐观锁版本号
    checkpoint_id: Option<String>,
}
```

## 七、参考项目

- **devops-agent**: `/Users/pengh/www/enjoy/devops-agent/backend/src/` — 提取源
- **LangChain**: 流式传输系统设计参考（stream_mode: updates/messages/custom）
