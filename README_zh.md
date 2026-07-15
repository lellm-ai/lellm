# LeLLM

> LeLLM 传递快乐。人嘛，最重要的就是开心。

**Rust 生产级 LLM 编排框架** —— 编译期类型安全、零成本抽象、可预测的运行时行为。

构建可靠的 AI 系统：Agent 循环、工具调用、工作流图、检查点、人工介入 —— 没有魔法，只有工程。

[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.4.10-green)](./CHANGELOG.md)

## 快速开始

```rust
use lellm::prelude::*;

// 1. Provider —— 自动读取 OPENAI_API_KEY
let provider = CodecProvider::load(OpenAICompatCodec::openai())?;

// 2. 定义工具
#[tool(name = "get_weather", description = "获取指定城市的天气")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// 3. 构建 Agent
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

// 4. 运行
let result = agent
    .invoke(vec![Message::user_text("上海天气如何？")])
    .await?;

match result.stop_reason {
    StopReason::Complete => println!("完成，共 {} 轮", result.iterations),
    StopReason::MaxIterationsReached => eprintln!("达到最大轮次"),
    _ => eprintln!("停止原因: {:?}", result.stop_reason),
}
```

**这就是一个完整的生产级 Agent 循环** —— 包含预算控制、工具重试、上下文压缩、流式输出。全部类型安全。全部显式可控。

---

## 为什么需要 LeLLM

大多数 AI 框架优化的是原型速度。**LeLLM 优化的是生产可靠性。**

构建真实的 AI 系统时，调 API 是最简单的部分。真正的挑战在于：

| 问题 | LeLLM 的方案 |
|---|---|
| Provider 差异（OpenAI / Anthropic / Gemini）| 统一 `ChatCodec` —— 一套 API，六个 Provider |
| Agent 循环失控 | 硬性的 `max_iterations` + Token 预算控制 |
| 上下文溢出 | 可插拔的压缩策略 |
| 工具执行失败 | 类型化的重试策略 + `ParallelSafety` 分类 |
| 流式输出部分失败 | 解耦的流式管道 —— 纯逻辑，无 IO |
| 状态一致性 | Checkpoint + Mutation Log + Trace 审计追踪 |
| 人工审批流程 | `BarrierNode` —— 暂停、决策、恢复 |
| 并行工作流 | `ParallelNode` —— 扇出/扇入 + 合并策略 |

---

## 与同类框架对比

| | **LeLLM** | **LangChain (Python)** | **LangGraph (Python)** | **Semantic Kernel** |
|---|---|---|---|---|
| **语言** | Rust | Python | Python | C#/Java/Python |
| **类型安全** | 编译期保证 | 运行时检查 | 运行时检查 | 部分 |
| **图引擎** | 内置，零 LLM 依赖 | 外部依赖 (LangGraph) | 内置 | 有限 |
| **Agent 循环** | ReAct = 内部图 | 手写 while 循环 | 状态机 | 线性链 |
| **检查点** | 内置，强类型 | 外部依赖 | 内置 | 有限 |
| **流式输出** | 解耦管道 | 依赖 Provider | 依赖 Provider | 依赖 Provider |
| **运行时开销** | 极小（无 GIL）| CPython 开销 | CPython 开销 | .NET 开销 |
| **部署目标** | Rust 能跑的任何地方 | 需要 Python 运行时 | 需要 Python 运行时 | 需要 .NET 运行时 |
| **理念** | 显式、可观测 | 约定优于配置 | DAG 优先 | SDK 风格 |

**LeLLM 就是为 Rust 从零设计的 LangGraph。**

---

## 为谁而建

### 适合你，如果你：

- **后端 / 基础设施工程师**，构建 AI 驱动的服务
- **平台团队**，构建 Agent 运行时或编排层
- **性能敏感型**应用（边缘、嵌入式、低延迟）
- 需要**确定性运行时行为**和可观测性的团队
- 想要**编译期类型保证**的 Rust 团队

**典型场景：**
- AI API 与网关
- 内部 Copilot
- Agent 运行时与多 Agent 编排
- 实时流式应用
- 长周期自主工作流

### 可能不适合你，如果：

- 主要在 Jupyter Notebook 中做实验
- 应用只是 `HTTP → LLM → return`（用 `reqwest` + `serde` 就够了）
- 想要无代码 / 低代码工作流
- 想通过 AI 项目学 Rust

当**编排复杂度**出现时，LeLLM 才开始发挥价值。

---

## 设计原则

### 类型安全优先

无效状态尽可能在编译期报错。`ToolArgs` 是强类型 struct，不是 `dict`。

### 显式优于魔法

重试、流式、预算、内存策略 —— 全部可观测、可配置。没有隐藏行为。

### 组合优于框架锁定

钻石架构 —— `lellm-graph` 和 `lellm-provider` 是平等层，都构建于 `lellm-core` 之上：

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

- **lellm-core** — 零运行时协议类型。可独立使用。
- **lellm-provider** — 仅 Provider 适配器。无 Graph、无 Agent。
- **lellm-graph** — 通用工作流引擎。**零 LLM 依赖。**
- **lellm-agent** — 组合 provider + graph。ReAct 循环是内部 Graph。

### Graph 是 Runtime，Agent 是 DSL

ReAct Agent 循环**不是**手写的 `while` 循环 —— 而是一个内部 Graph：

```
START → budget_check ──(充足)──→ [llm] → [post_llm_check]
         │                          │              │
      (压缩) → [compactor]         │       有工具 → [tool] → budget_check (循环)
                                   │       无工具 → [end]
                              (思考输出)
```

这意味着所有 Graph 功能 —— 检查点、Barrier、并行执行、Trace —— Agent 都能直接用。

---

## 功能概览

### Provider 抽象

一套统一 API 调用所有 LLM Provider：

| Provider | Codec | 流式 | 工具调用 |
|---|---|---|---|
| OpenAI | `OpenAICompatCodec::openai()` | ✅ | ✅ |
| Anthropic | `AnthropicCodec` | ✅ | ✅ |
| Google | `GoogleCodec` | ✅ | ✅ |
| DeepSeek | `OpenAICompatCodec::deepseek()` | ✅ | ✅ |
| NVIDIA | `OpenAICompatCodec::nvidia()` | ✅ | ✅ |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` | ✅ | ✅ |

Provider 集成分离为三个关注点：`ChatCodec + ModelCapabilities + ProviderMeta`。

### Agent 运行时

- **ReAct 循环** 作为内部 Graph —— 不是 `while` 循环
- **工具调用** 带类型化参数，自动生成 JSON Schema
- **上下文预算** —— 硬性 Token 限制，可插拔压缩策略
- **重试策略** —— 可配置退避，`ParallelSafety` 分类
- **流式输出** —— `AgentStream` + `AgentEvent` 事件流
- **降级策略** —— LLM 错误时的优雅降级

### Graph 编排

`lellm-graph` 是通用工作流引擎 —— 定位类似 LangGraph / Temporal / Prefect。
**对 LLM 零依赖。**

| 节点 | 用途 |
|---|---|
| `TaskNode` | 简单函数节点 |
| `ConditionNode` | 条件分支 —— 根据状态路由 |
| `BarrierNode` | 人工介入 —— 暂停、决策、恢复 |
| `ParallelNode` | 扇出/扇入 —— 并行分支 + 合并策略 |

三种执行模式：**阻塞**、**流式**（channel 事件流）、**内联**（零 channel 开销）。

### 检查点与追踪

- **Checkpoint** —— 在节点边界持久化状态，从故障点恢复
- **Mutation Log** —— 每次状态变更记录为类型化 Mutation
- **执行 Trace** —— 每一步的审计追踪，可导出为 JSON
- **Barrier Re-Wait** —— 恢复时重新等待人工审批

### 工具系统

两种定义工具的方式：

```rust
// 方式一：#[tool] 宏 —— 95% 的场景
#[tool(name = "get_weather", description = "获取天气")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// 方式二：#[derive(Tool)] —— 完全控制 struct
#[derive(Deserialize, JsonSchema, Tool)]
struct GetWeatherArgs {
    /// 城市名称
    city: String,
}
```

工具**永远不是** `dict` —— `ToolArgs` 是强类型的 Rust struct，自动生成 JSON Schema。

### MCP 集成

内置 MCP 客户端/服务端支持，动态工具目录、冲突解决、注册表管理。

---

## 安装

```toml
[dependencies]
# 默认：core + provider 适配器
lellm = "0.4"

# Agent 运行时（包含 core + graph + provider + agent）
lellm = { version = "0.4", features = ["agent"] }

# 全部启用
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

---

## 架构

### Crate 布局

```
lellm/
├── lellm/               # 门面 crate —— 统一入口
├── lellm-core/          # 协议类型（Message, ChatRequest, LlmError）
│   └── deps: serde, serde_json, schemars, thiserror
├── lellm-provider/      # Provider 适配器（OpenAI, Anthropic, Google 等）
│   └── deps: lellm-core + reqwest + tokio
├── lellm-graph/         # 通用工作流引擎（无 LLM 依赖）
│   └── deps: lellm-core + tokio
├── lellm-agent/         # Agent 运行时（组合 provider + graph）
│   └── deps: lellm-core + lellm-provider + lellm-graph
├── lellm-derive/        # 派生宏 + 属性宏（proc-macro）
└── lellm-mcp/           # MCP（Model Context Protocol）客户端/服务端
    └── deps: lellm-core + optional lellm-agent
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
