# LeLLM

[English](./README.md) | 中文

> LeLLM 传递快乐。人嘛，最重要的就是开心。

**Rust 中的图原生 Agent 编排框架。**

将 AI Agent 构建为类型化的有向图 —— 编译期类型安全、持久化检查点、人工介入、无需外部服务。

[![crates.io](https://img.shields.io/crates/v/lellm.svg)](https://crates.io/crates/lellm)
[![License](https://img.shields.io/crates/l/lellm)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)

```bash
cargo add lellm
```

---

## 快速开始

```rust
use lellm::prelude::*;
use std::sync::Arc;

// 1. Provider —— 自动读取 OPENAI_API_KEY
let provider = CodecProvider::load(OpenAICompatCodec::openai())?;

// 2. 定义工具 —— #[tool] 自动从函数签名生成 JSON Schema
#[tool(name = "get_weather", description = "获取指定城市的天气")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// 3. 构建并运行 Agent
let model = ResolvedModel {
    provider: Arc::new(provider),
    model: "gpt-4o".into(),
    context_window: None,
};

let agent = AgentBuilder::new(model)
    .system("你是一个有用的助手。")
    .tool(get_weather_tool())
    .max_iterations(10)
    .compile();

let result = agent
    .invoke(vec![Message::user_text("上海天气如何？")])
    .await?;

match result.stop_reason {
    StopReason::Complete => println!("完成，共 {} 轮", result.iterations),
    StopReason::MaxIterationsReached => eprintln!("达到最大轮次"),
    _ => eprintln!("停止原因: {:?}", result.stop_reason),
}
```

这就是一个完整的 **ReAct Agent 循环** —— 包含工具调用、预算控制、重试策略、上下文压缩。没有隐藏状态。没有运行时魔法。每个类型都在编译期检查。

---

## 为什么选择 LeLLM

大多数 AI 框架优化的是原型速度。**LeLLM 优化的是生产可靠性。**

构建 AI 系统时，调 API 不是难点 —— 让系统在高负载下保持正确才是：

| 挑战 | LeLLM 的方案 |
|---|---|
| Provider 差异 | 统一 `ChatCodec` —— 一套 API，六个 Provider |
| Agent 循环失控 | 硬性的 `max_iterations` + Token 预算，运行时强制 |
| 上下文溢出 | 可插拔压缩策略，可观测的 Token 计数 |
| 工具执行失败 | 类型化的重试策略 + `ParallelSafety` 并发分类 |
| 流式部分失败 | 解耦的流式管道 —— 纯逻辑，零 IO 耦合 |
| 状态一致性 | Checkpoint + Mutation Log + 执行 Trace 审计追踪 |
| 人工审批门 | `BarrierNode` —— 暂停、决策、从精确状态恢复 |
| 并行工作流 | `ParallelNode` —— 扇出/扇入 + 类型化合并策略 |

---

## 核心优势

### Graph 是 Runtime，Agent 是 DSL

LeLLM 中每个 Agent 都是编译后的有向图。ReAct 循环不是手写的 `while` —— 而是：

```
START → budget_check ──(充足)──→ [llm] → [post_llm_check]
         │                          │              │
      (压缩) → [compactor]         │       有工具 → [tool] → budget_check
                                   │       无工具 → [end]
```

这意味着**所有 Graph 功能对 Agent 生效**：检查点、Barrier、并行执行、追踪。无需额外代码即可获得持久化执行。

### 持久化执行

构建能从故障中恢复的 Agent，从精确的断点继续执行：

```rust
use lellm::prelude::*;

// 创建带 Graph 的会话
let session = ExecutionSession::new(state, graph.clone());

// 保存 Checkpoint —— 可序列化到任意后端（文件、S3、Redis）
let checkpoint = session.checkpoint();

// ... 崩溃、重启、部署 ...

// 恢复 —— 自动校验 graph hash，不匹配则拒绝恢复
let restored = ExecutionSession::restore(checkpoint, graph)?;
```

- **Checkpoint** —— 在每个节点边界持久化类型化状态
- **Mutation Log** —— 每次状态变更记录为类型化 Mutation
- **执行 Trace** —— 完整审计追踪，可导出为 JSON
- **Barrier Re-Wait** —— 恢复时重新等待人工审批

### 人工介入

在任意点暂停 Agent 执行，检查或修改状态，然后继续：

```rust
use lellm::prelude::*;

let graph = GraphBuilder::<State>::new("workflow")
    .start("research")
    .node("research", TaskNode::new(research_fn))
    .node("approve", BarrierNode::new("human_review")
        .timeout(Duration::from_secs(300)))
    .node("act", TaskNode::new(action_fn))
    .edge("research", "approve")
    .edge("approve", "act")
    .end("act")
    .build()?;

// BarrierNode 暂停执行并发送 BarrierId。
// 你的控制平面决定：批准、拒绝、修改或重路由。
handle.decide(barrier_id, BarrierDecision::Approve).await?;
```

### 类型安全的状态

状态是强类型结构体，不是 `dict`。工具参数是 Rust struct，自动生成 JSON Schema。无效状态在编译时报错。

```rust
// 工具参数强类型 —— 不用猜"这个工具有什么参数"
#[derive(Deserialize, JsonSchema, Tool)]
struct SearchArgs {
    /// 搜索关键词
    query: String,
    /// 最大结果数
    #[serde(default = "default_limit")]
    limit: u32,
}
```

### 多 Provider，一套 API

| Provider | Codec | 流式 | 工具调用 |
|---|---|---|---|
| OpenAI | `OpenAICompatCodec::openai()` | ✅ | ✅ |
| Anthropic | `AnthropicCodec` | ✅ | ✅ |
| Google | `GoogleCodec` | ✅ | ✅ |
| DeepSeek | `OpenAICompatCodec::deepseek()` | ✅ | ✅ |
| NVIDIA | `OpenAICompatCodec::nvidia()` | ✅ | ✅ |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` | ✅ | ✅ |

改一行代码切换 Provider。`ChatCodec` 抽象处理协议编码、流式解析、能力协商。

---

## 常见模式

### 流式 Agent 执行

```rust
use futures_util::StreamExt;

let mut stream = agent.invoke_stream(vec![Message::user_text("分析这段代码...")]);

while let Some(event) = stream.next().await {
    match event {
        AgentEvent::Provider(ProviderEvent::Token { token }) => print!("{}", token),
        AgentEvent::ToolStart { name, .. } => eprintln!("\n🔧 调用: {}", name),
        AgentEvent::ToolEnd { result, .. } => eprintln!("✅ 结果: {:?}", result),
        AgentEvent::LoopEnd { result } => {
            eprintln!("\n完成，共 {} 轮", result.iterations);
        }
        _ => {}
    }
}
```

### 自定义工作流图

`lellm-graph` **对 LLM 零依赖** —— 可作为通用工作流引擎使用：

```rust
use lellm::prelude::*;

let graph = GraphBuilder::<State>::new("rag_pipeline")
    .start("fetch")
    .node("fetch", TaskNode::new(fetch_documents))
    .node("analyze", TaskNode::new(analyze_fn))
    .node("review", BarrierNode::new("quality_check"))
    .node("publish", TaskNode::new(publish_result))
    .edge("fetch", "analyze")
    .edge("analyze", "review")
    // 条件边：可信来源跳过审核
    .edge_if("review", "publish", |state: &State| is_trusted(state))
    .edge_fallback("review", "fetch")  // 拒绝时回退重新获取
    .end("publish")
    .build()?;
```

### 并行分支执行

```rust
let parallel = ParallelNode::builder()
    .branch("translate", TaskNode::new(translate_fn))
    .branch("summarize", TaskNode::new(summarize_fn))
    .branch("extract", TaskNode::new(extract_fn))
    .error_strategy(ParallelErrorStrategy::CollectAll)
    .build();
```

---

## 与同类框架对比

| | **LeLLM** | **LangGraph (Python)** | **AutoGen** |
|---|---|---|---|
| **语言** | Rust | Python | Python/TS |
| **类型安全** | 编译期（struct 状态、类型化 Mutation）| 运行时（TypedDict） | 最小 |
| **Agent = Graph** | 是 —— ReAct 是编译后的内部图 | 是 —— StateGraph | 否 —— 线性链 |
| **图引擎** | 内置，零 LLM 依赖 | 内置 | 外部依赖 |
| **检查点** | 内置，强类型，带 Mutation Log | 内置（checkpointer） | 有限 |
| **人工介入** | `BarrierNode` 带决策路由 | `interrupt()` | 有限 |
| **流式输出** | 解耦管道（纯逻辑，无 IO） | 多种流式模式 | 基础 |
| **运行时** | 无 GIL，真正并行 | asyncio（单线程事件循环） | asyncio |
| **部署** | Rust 能跑的任何地方（服务器、边缘、WASM） | 需要 Python 运行时 | 需要 Python/Node |
| **可观测性** | 内置 Trace + Mutation Log | LangSmith（付费服务） | LangChain 追踪 |
| **理念** | 显式、可观测、类型安全 | 约定优于配置 | 对话模式 |

**关键区别：** LeLLM 的图引擎**对 LLM 零依赖** —— `lellm-graph` 仅依赖 `lellm-core` 的协议类型。可观测性内置（Trace + Mutation Log），不是付费云服务。

---

## 为谁而建

### 适合你，如果你：

- **后端 / 基础设施工程师**，构建 AI 驱动的服务
- **平台团队**，构建 Agent 运行时或编排层
- 构建**性能敏感型**应用（边缘、嵌入式、低延迟）
- 需要**确定性运行时行为**和内置可观测性的团队
- 想要**编译期类型保证**的 Rust 团队

**典型场景：** AI API 与网关、内部 Copilot、Agent 运行时、多 Agent 编排、实时流式应用、长周期自主工作流。

### 可能不适合你，如果：

- 主要在 Jupyter Notebook 中做实验
- 应用只是 `HTTP → LLM → return` —— `reqwest` + `serde` 就够了
- 想要无代码 / 低代码工作流
- 想通过 AI 项目学 Rust

当**编排复杂度**出现时，LeLLM 才开始发挥价值。

---

## 安装

```bash
cargo add lellm
```

或按需选择功能：

```toml
[dependencies]
# 默认：core + provider 适配器（直接调用 LLM）
lellm = "0.4"

# Agent 运行时（core + graph + provider + agent）
lellm = { version = "0.4", features = ["agent"] }

# 全部启用（graph + provider + agent + mcp + derive）
lellm = { version = "0.4", features = ["full"] }
```

**Feature 依赖矩阵：**

| Feature | 包含 |
|---|---|
| `core` | lellm-core |
| `provider` | core + lellm-provider |
| `graph` | core + lellm-graph |
| `agent` | core + graph + provider + lellm-agent |
| `mcp` | core + graph + lellm-mcp |
| `derive` | lellm-derive |
| `full` | graph + provider + agent + mcp + derive |

**系统要求：** Rust 2024 edition，stable 工具链。

---

## 架构

### 钻石架构

```
              lellm-core（协议类型）
             /                \
            /                  \
  lellm-provider           lellm-graph
  （LLM 适配器）            （工作流引擎）
            \                  /
             \                /
      lellm-agent（ReAct = 内部 Graph）
```

- **lellm-core** —— 零运行时协议类型（`Message`、`ChatRequest`、`LlmError`）。可独立使用。
- **lellm-provider** —— 仅 Provider 适配器。无 Graph、无 Agent。与 core 搭配独立使用。
- **lellm-graph** —— 通用工作流引擎。**零 LLM 依赖。**与 core 搭配独立使用。
- **lellm-agent** —— 组合 provider + graph。ReAct 循环是内部 Graph。
- **lellm-mcp** —— MCP 客户端/服务端。独立的协议域。
- **lellm-derive** —— 过程宏 crate（`#[tool]`、`#[derive(Tool)]`）。

### Crate 布局

```
lellm/
├── lellm/               # 门面 crate —— 统一入口
├── lellm-core/          # 协议类型（serde, thiserror）
├── lellm-provider/      # Provider 适配器（core + reqwest + tokio）
├── lellm-graph/         # 工作流引擎（core + tokio，无 LLM）
├── lellm-agent/         # Agent 运行时（core + provider + graph）
├── lellm-derive/        # 派生宏 + 属性宏（proc-macro）
└── lellm-mcp/           # MCP 客户端/服务端（core + 可选 agent）
```

### 解耦的流式管道

`stream/` 完全不知道 `reqwest` 或 `tokio channel`：

```
CodecProvider（HTTP, channel）
       ↓
process_stream（Stream<Item=Result<Bytes>>, EventSink）
       ↓
SseParser + Codec + Accumulator（纯逻辑，无 IO）
```

---

## 路线图

| 版本 | 范围 | 状态 |
|---|---|---|
| **v0.1** | Provider 抽象、流式执行、工具执行、预算控制 | ✅ 已完成 |
| **v0.2** | Graph 编排、Provider 扩展 API、内存架构 | ✅ 已完成 |
| **v0.3** | Agent graph runtime —— ReAct 循环、Barrier、多 Agent 协调 | ✅ 已完成 |
| **v0.4** | ReAct = 内部图、类型化状态、检查点、Trace、并行执行 | ✅ 已完成 |
| **v0.5** | Graph 是 Runtime、Agent 是 DSL、Checkpoint Projection、执行 Session | 🔜 进行中 |
| **v0.6** | 分布式执行、可视化可观测、人工介入 SDK | 计划中 |

---

## 理念

像构建数据库、网关、分布式服务一样构建 AI 系统：

**显式、可观测、类型安全。**

---

## 相关链接

- [产品蓝图](./docs/BLUEPRINT.md) —— 产品蓝图与核心 API 契约
- [设计文档](./docs/DESIGN.md) —— 关键设计决策的为什么与如何实现
- [LangGraph vs LeLLM Graph](./docs/langgraph-vs-lellm-graph.md) —— 编排架构深度对比

## 许可证

MIT
