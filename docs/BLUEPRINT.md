# lellm v0.1 产品蓝图

> 版本：v0.1 | 日期：2026-06-01 | 状态：已确认

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
| `lellm-core` | 协议对象 | Message, ContentBlock(Text/Thinking/Image/ToolCall/ToolResult), ChatRequest/Response, ToolDefinition, TokenUsage, LlmError |
| `lellm-provider` | Provider trait + 适配器 | LlmProvider trait, OpenAI/Anthropic/NVIDIA/DeepSeek/VLLM/LLaMA, ModelRouter, ProviderBuilder, StreamEvent |
| `lellm-agent` | Agent 运行时 | ToolRegistry, ToolExecutor, ToolUseLoop, ParallelSafety, RetryPolicy, LoopDetector, FallbackStrategy, ShortTermMemory, LongTermMemory, MemoryStore |
| `lellm-macros` | 派生宏 | `#[derive(ToolDefinition)]` — JSON Schema 生成 + 参数反序列化 |

### 不包含（v0.2+）

- Graph/Node/Edge 编排层（v0.1 完全不包含）
- MCP Client/Server（v0.1 仅预留 `ToolSource::Mcp`）
- Sandbox（v0.1 暂不提取）
- Harness Orchestrator（v0.1 暂不提取）

## 三、Workspace 结构

```
lellm/
├── Cargo.toml                  # workspace root
├── docs/
│   └── BLUEPRINT.md            # 本文件
├── lellm-core/                 # 协议对象，零运行时依赖
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── message.rs          # Message, ContentBlock
│       ├── request.rs          # ChatRequest, ToolDefinition, ToolChoice
│       ├── response.rs         # ChatResponse, ToolCall, ToolResult, TokenUsage
│       └── error.rs            # LlmError, ToolError, MemoryError, ParseError
├── lellm-provider/             # Provider trait + 适配器
│   ├── Cargo.toml              # features: openai, anthropic, openai-compat
│   └── src/
│       ├── lib.rs              # LlmProvider trait, StreamEvent, StreamMode
│       ├── builder.rs          # ProviderBuilder
│       ├── router.rs           # ModelRouter, TaskLevel
│       └── providers/
│           ├── mod.rs
│           ├── base.rs         # GenericProvider<Adapter>, ProviderAdapter trait
│           ├── openai.rs       # OpenAIAdapter (feature = "openai")
│           ├── anthropic.rs    # AnthropicAdapter (feature = "anthropic")
│           └── openai_compat.rs # OpenAI兼容 (NVIDIA/DeepSeek/VLLM/LLaMA)
│           └── mock.rs         # MockProvider (测试用)
├── lellm-agent/                # Agent 运行时
│   ├── Cargo.toml              # features: sqlite
│   └── src/
│       ├── lib.rs
│       ├── agent.rs            # Agent 主结构
│       ├── tools/
│       │   ├── mod.rs
│       │   ├── registry.rs     # ToolRegistry, ToolSource, ToolSearchResult
│       │   ├── executor.rs     # ToolExecutor, ParallelSafety, ToolRegistration
│       │   ├── loop_.rs        # ToolUseLoop (含流式 execute_stream)
│       │   ├── retry.rs        # ToolErrorKind, RetryPolicy, BackoffStrategy
│       │   ├── loop_detector.rs # LoopDetector, ToolCallFingerprint
│       │   ├── fallback.rs     # FallbackStrategy trait, FallbackReason, DefaultFallback
│       │   └── signal_voter.rs # SignalVoter, NegativeSignal
│       └── memory/
│           ├── mod.rs
│           ├── short_term.rs   # ShortTermMemory (环形缓冲)
│           ├── long_term.rs    # LongTermMemory
│           └── store.rs        # MemoryStore (SQLite, feature = "sqlite")
└── lellm-macros/               # 派生宏
    ├── Cargo.toml              # proc-macro = true
    └── src/
        └── lib.rs              # #[derive(ToolDefinition)]
```

## 四、核心设计决策

### 4.1 Crate 划分原则

**Crate 拆分准则：** 只有当某个模块未来可能被单独引用（被其他项目依赖）时，才值得拆成独立 crate。

**Core 原则：**
- 零 Provider 依赖
- 零 Runtime 依赖
- 尽量少 Feature
- 能被所有 crate 引用

**v0.1 选择 4 个 crate 而非 5 个的原因：**

1. **Memory 不单独 crate** — ShortTermMemory 本质是 `VecDeque<Message>`；LongTermMemory 本质是 SQLite 存储。两者都与 Agent Loop 高度耦合，很少被单独引用。
2. **Tool System 不单独 crate** — ToolRegistry、ToolExecutor、ToolUseLoop、Fallback、LoopDetector 全部是 Agent Runtime 的组成部分。LangChain、AutoGen、OpenAI Agents SDK 都没有将 Tool 独立成包。
3. **真正独立的是协议对象** — Tool trait、ToolDefinition、ToolCall、ToolResult 放在 `lellm-core`，作为统一抽象层（类似 openai-types / anthropic-types 的统一版）。

### 4.2 发布策略 — 分阶段

v0.1 只确保 **LLM ↔ Tool Call** 核心闭环独立可运行且通过测试。Graph 编排和 MCP 建立在此闭环之上。

### 4.3 Feature Gate

provider 适配器通过 feature flag 可选编译，用户只编译需要的 provider。避免拆成独立 crate 的过度设计。

### 4.4 剥离策略 — 复制 + 净化

1. 读取 devops-agent 源文件
2. 手动移除所有对 devops-agent 内部模块的引用
3. 用 trait/接口替代具体类型依赖
4. 写入 lellm workspace，编写独立测试
5. 最后让 devops-agent `Cargo.toml` 依赖 `lellm-*`，删除原代码

### 4.5 Memory 存储 — SQLite 直接绑定

v0.1 只做 SQLite 后端，feature gate 开关。等真正需要第二个存储时再抽象。

### 4.6 流式支持 — 全程事件流式（LangChain 模式）

```rust
pub type LlmStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send>>;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    LlmStart { model: String, messages_count: usize },
    LlmToken { token: String },
    LlmEnd { tool_calls: Vec<ToolCall> },
    ToolStart { tool_call_id: String, name: String },
    ToolEnd { tool_call_id: String, result: String },
    Custom { data: serde_json::Value },
}
```

**Trait 暴露 `Stream`，Provider 内部用 `mpsc`：**
- `LlmProvider::llm_call_stream` 返回 `Result<LlmStream, LlmError>`
- Provider 内部仍用 `mpsc::channel(32)` 实现，转换为 BoxStream 返回
- 消费者可以用 tokio-stream 的 `.merge()`、`.take()` 等组合操作

ToolUseLoop 提供两个接口：

```rust
impl ToolUseLoop {
    // 非流式
    pub async fn execute(self) -> Result<ToolUseResult, LlmError>;

    // 流式（返回事件接收器）
    pub fn execute_stream(
        self,
        mode: StreamMode,
    ) -> tokio::sync::mpsc::Receiver<Result<StreamEvent, LlmError>>;
}
```

内部累积 + 外部流式：纯文本部分实时发送 `LlmToken`，tool_calls 在 LLM 返回完成后统一解析并原子执行。

### 4.6.5 ContentBlock 统一协议

`ContentBlock` 是 Message 和 ChatResponse 的唯一内容载体：

```rust
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    Image { source: ImageSource },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}
```

**ChatResponse 与 Message::Assistant 对齐：**

```rust
pub struct ChatResponse {
    pub content: Vec<ContentBlock>,  // 与 Message::Assistant 一致
    pub usage: TokenUsage,
    pub raw: serde_json::Value,      // provider 特有字段兜底
}
```

`ContentBlock::ToolCall` 是唯一真相源（Source of Truth），不再有独立的 `tool_calls` 字段。
`raw` 保留用于 provider 特有字段（stop_reason, safety_ratings 等）。

### 4.7 工具并行执行 — 按 ParallelSafety 分级

```rust
pub enum ParallelSafety {
    Safe,                  // 可安全并行
    CategoryExclusive,     // 同类互斥（类别从 tool.category() 读取）
    Exclusive,             // 完全互斥
}

pub struct ToolCategory(Cow<'static, str>);
impl ToolCategory {
    pub const FILE_IO: Self = ...;
    pub const NETWORK: Self = ...;
    pub const DATABASE: Self = ...;
    pub fn custom(name: impl Into<Cow<'static, str>>) -> Self;
}
```

执行器逻辑：
- `Safe` → 直接 `tokio::join!` 并行
- `CategoryExclusive` → 按 `category()` 分组，组内串行、组间并行
- `Exclusive` → 全局逐个串行

### 4.8 Provider 架构 — 三层分离

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
    fn parse_stream_chunk(&self, chunk: &[u8])
        -> Result<Option<StreamChunk>, LlmError>;
}

// ─── 自动实现 ───
#[async_trait]
impl<A: ProviderAdapter + Send + Sync> LlmProvider for GenericProvider<A> { ... }
```

`OpenAICompatAdapter` 一个实现覆盖 7+ provider（OpenAI, NVIDIA, DeepSeek, VLLM, LLaMA, LM Studio, Ollama, OpenRouter），仅 `base_url` 不同。

用户永远只面对 `LlmProvider`，无需创建 `XxxProvider` 包装类型。

### 4.9 ModelRouter — 三级路由（配置解析器）

```rust
pub enum TaskLevel { Flash, Standard, Pro }

pub struct RouteEntry {
    pub provider_id: String,
    pub model: String,
}

pub struct ModelRouter {
    routes: HashMap<TaskLevel, RouteEntry>,
}

impl ModelRouter {
    pub fn resolve(&self, level: TaskLevel) -> Option<&RouteEntry>;
}
```

**Router 不持有 `Arc<dyn LlmProvider>`** — 只做配置解析，返回 `RouteEntry` 由外部组装。

### 4.9.1 ProviderRegistry — Provider 容器

```rust
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn register(&self, id: &str, provider: Arc<dyn LlmProvider>);
    pub fn get(&self, id: &str) -> Option<Arc<dyn LlmProvider>>;
}
```

### 4.9.2 使用模式

```rust
let route = router.resolve(TaskLevel::Flash)?;
let provider = registry.get(&route.provider_id)?;
provider.call(request.with_model(route.model.clone()));
```

### 4.10 配置管理 — 构建器模式

```rust
let registry = ProviderRegistry::new();
registry.register("openai", Arc::new(
    GenericProvider::<OpenAICompatAdapter>::new(...)
));
registry.register("anthropic", Arc::new(
    GenericProvider::<AnthropicAdapter>::new(...)
));
```

库不读配置文件，配置加载留给上层应用。

### 4.11 Tool 声明 — Schema 与 Runtime 分离

**核心原则：** Tool Schema（给 LLM 看）与 Tool Runtime（给 Agent 执行）是完全不同的职责，不应绑死。

```rust
// ─── Schema 层 — derive 宏生成 JSON Schema + 反序列化 ───
#[derive(ToolDefinition)]
pub struct ReadFileArgs {
    /// 文件路径
    pub path: String,
}

// 宏生成：
// impl ReadFileArgs {
//     fn schema() -> ToolDefinition { ... }
//     fn from_args(value: &serde_json::Value) -> Result<Self, ToolError> { ... }
// }

// ─── Runtime 层 — 用户手动实现执行逻辑 ───
#[async_trait]
pub trait ToolExecutor {
    type Args;

    fn definition(&self) -> ToolDefinition;
    fn category(&self) -> Option<ToolCategory>;
    fn parallel_safety(&self) -> ParallelSafety;
    async fn execute(&self, args: Self::Args, ctx: &ToolContext)
        -> Result<ToolResult, ToolError>;
}

// 用户实现
pub struct ReadFileTool;
#[async_trait]
impl ToolExecutor for ReadFileTool {
    type Args = ReadFileArgs;

    fn definition(&self) -> ToolDefinition { ReadFileArgs::schema() }
    fn category(&self) -> Option<ToolCategory> { Some(ToolCategory::FILE_IO) }
    fn parallel_safety(&self) -> ParallelSafety { ParallelSafety::CategoryExclusive }

    async fn execute(&self, args: ReadFileArgs, _ctx: &ToolContext)
        -> Result<ToolResult, ToolError>
    {
        // ... 执行文件读取
    }
}

// ─── Registry 层 — 类型擦除，统一存储 ───
// ToolRegistry 内部存储 Box<dyn DynToolExecutor>
// 通过 erased_serde 或类似机制擦除 Args 类型参数
```

**为什么这样设计：**
- Local Tool、MCP Tool、Remote Tool、HTTP Tool 未来都可接入同一 Registry
- Schema 与 Runtime 解耦，避免框架把三合一绑死导致的重构痛苦

### 4.12 循环检测 — 指纹去重 + 阈值触发

```rust
pub struct LoopDetector {
    history: Vec<ToolCallFingerprint>,
    threshold: usize,
}

#[derive(Hash, Eq, PartialEq)]
struct ToolCallFingerprint {
    tool_name: String,
    normalized_args: String,  // JSON 键排序 + 空白去除
}
```

相同指纹连续出现超过阈值 → 注入系统提示干预。

### 4.13 Fallback 策略 — Agent Runtime 统一恢复机制

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
}

pub enum FallbackAction {
    Retry,                       // 原样重试
    RetryWithMessages(Vec<Message>), // 注入消息后重试
    SwitchProvider(RouteEntry),  // 切换 provider
    Complete(ChatResponse),      // 直接返回响应
    Abort,                       // 放弃
}

#[async_trait]
pub trait FallbackStrategy {
    async fn handle(&self, ctx: &FallbackContext) -> FallbackAction;
}
```

**v0.1 DefaultFallback：** 仅支持 `Retry` / `Abort`
**v0.2+：** 扩展 `SwitchProvider`, `RetryWithMessages`, `Complete`

### 4.14 ContentBlock — 核心层极简

见 4.6.5 节的统一 ContentBlock 设计。

`cache_control` 等 provider 特有标记下沉到 provider adapter 层。

### 4.15 错误体系 — 每层自定义 Error

```rust
// lellm-core
pub enum LellmError {
    Llm(LlmError),
    Tool(ToolError),
    Memory(MemoryError),
    Parse(ParseError),
}
```

库边界用自定义 Error，内部用 anyhow。

### 4.16 异步运行时 — 分层绑定

- `lellm-core`：零运行时依赖（仅 serde + serde_json）
- `lellm-provider`：绑定 tokio + reqwest
- `lellm-agent`：绑定 tokio
- `lellm-macros`：proc-macro，零运行时

### 4.17 测试策略 — 三层 + Mock Provider

| 层级 | 内容 |
|------|------|
| 单元测试 | 每个 crate 内部测试 |
| 集成测试 | `tests/` 目录，跨 crate 集成场景 |
| 示例项目 | `examples/` 可运行 demo |

MockProvider 通过 feature = "mock" 提供，支持预设响应和 tool_call 注入。

## 五、版本路线图

| 版本 | 范围 | 状态 |
|------|------|------|
| **v0.1** | core + provider + agent + macros | 蓝图已确认 |
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

### v0.2 — lellm-graph 详细规划

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

### v0.2 — lellm-graph 详细规划

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

## 六、参考项目

- **devops-agent**: `/Users/pengh/www/enjoy/devops-agent/backend/src/` — 提取源
- **LangChain**: 流式传输系统设计参考（stream_mode: updates/messages/custom）
