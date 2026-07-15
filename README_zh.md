# LeLLM

[English](./README.md) | 中文

> LeLLM 传递快乐。人嘛，最重要的就是开心。

**用 Rust 将 AI Agent 构建为类型化的有向图。**

编译期类型安全、持久化检查点、人工介入、无需外部服务。

[![crates.io](https://img.shields.io/crates/v/lellm.svg)](https://crates.io/crates/lellm)
[![License](https://img.shields.io/crates/l/lellm)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)

```rust
use lellm::prelude::*;
use std::sync::Arc;

let provider = CodecProvider::load(OpenAICompatCodec::openai())?;

#[tool(name = "get_weather", description = "获取指定城市的天气")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

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

完整的 **ReAct Agent 循环** —— 工具调用、预算控制、重试策略、上下文压缩。每个类型编译期检查。

---

## 为什么选择 LeLLM

大多数 AI 框架优化的是原型速度。**LeLLM 优化的是生产可靠性。**

| 挑战 | LeLLM 的方案 |
|---|---|
| Provider 差异 | 统一 `ChatCodec` —— 一套 API，六个 Provider |
| Agent 循环失控 | 硬性的 `max_iterations` + Token 预算 |
| 上下文溢出 | 可插拔压缩，可观测的 Token 计数 |
| 工具执行失败 | 类型化重试策略 + `ParallelSafety` 分类 |
| 状态一致性 | Checkpoint + Mutation Log + 执行 Trace |
| 人工审批门 | `BarrierNode` —— 暂停、决策、恢复 |
| 并行工作流 | `ParallelNode` —— 扇出/扇入 + 类型化合并 |

---

## 核心概念

### Graph 是 Runtime，Agent 是 DSL

每个 Agent 都是编译后的有向图。ReAct 循环不是 `while` —— 而是真正的图：

```
START → budget_check ──(充足)──→ [llm] → [post_llm_check]
         │                          │              │
      (压缩) → [compactor]         │       有工具 → [tool] → budget_check
                                   │       无工具 → [end]
```

所有 Graph 功能 —— 检查点、Barrier、并行执行、追踪 —— 对 Agent 自动生效。

### 持久化执行

在节点边界持久化状态，从精确故障点恢复，自动校验 graph hash：

```rust
let checkpoint = session.checkpoint();
// ... 崩溃、重启、部署 ...
let restored = ExecutionSession::restore(checkpoint, graph)?;
```

### 人工介入

在任意节点暂停，检查或修改状态，决定批准 / 拒绝 / 修改 / 重路由：

```rust
let graph = GraphBuilder::<State>::new("workflow")
    .start("research")
    .node("research", TaskNode::new(research_fn))
    .node("approve", BarrierNode::new("human_review").timeout(Duration::from_secs(300)))
    .node("act", TaskNode::new(action_fn))
    .edge("research", "approve")
    .edge("approve", "act")
    .end("act")
    .build()?;

// Barrier 暂停执行 —— 控制平面做出决策：
handle.decide(barrier_id, BarrierDecision::Approve).await?;
```

### 全部类型安全

状态是 struct，不是 `dict`。工具参数是 Rust 类型，自动生成 JSON Schema。无效状态编译时报错。

### 多 Provider，一套 API

| Provider | Codec | 流式 | 工具调用 |
|---|---|---|---|
| OpenAI | `OpenAICompatCodec::openai()` | ✅ | ✅ |
| Anthropic | `AnthropicCodec` | ✅ | ✅ |
| Google | `GoogleCodec` | ✅ | ✅ |
| DeepSeek | `OpenAICompatCodec::deepseek()` | ✅ | ✅ |
| NVIDIA | `OpenAICompatCodec::nvidia()` | ✅ | ✅ |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` | ✅ | ✅ |

---

## 安装

```bash
cargo add lellm
```

```toml
[dependencies]
# 默认：core + provider 适配器
lellm = "0.4"

# Agent 运行时（core + graph + provider + agent）
lellm = { version = "0.4", features = ["agent"] }

# 全部启用
lellm = { version = "0.4", features = ["full"] }
```

| Feature | 包含 |
|---|---|
| `provider`（默认） | core + LLM 适配器 |
| `graph` | 独立工作流引擎 —— 零 LLM 依赖 |
| `agent` | 完整 Agent 运行时 —— ReAct + 工具 + 检查点 |
| `mcp` | MCP 客户端/服务端 |
| `derive` | `#[tool]` 和 `#[derive(Tool)]` 宏 |
| `full` | 全部 |

**系统要求：** Rust 2024 edition，stable 工具链。

---

## 架构

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

- **lellm-core** —— 协议类型。可独立使用。
- **lellm-provider** —— Provider 适配器。无 Graph、无 Agent。
- **lellm-graph** —— 工作流引擎。**零 LLM 依赖。**
- **lellm-agent** —— 组合 provider + graph。
- **lellm-mcp** —— MCP 客户端/服务端。
- **lellm-derive** —— 过程宏。

每个 crate 可独立使用。

---

## 与同类框架对比

| | **LeLLM** | **LangGraph** |
|---|---|---|
| 语言 | Rust | Python |
| 类型安全 | 编译期 | 运行时（TypedDict） |
| Agent = Graph | 是 —— 编译后的内部图 | 是 —— StateGraph |
| 图引擎 | 内置，零 LLM 依赖 | 内置 |
| 检查点 | 内置，强类型，Mutation Log | 内置 |
| 人工介入 | `BarrierNode` 带路由 | `interrupt()` |
| 流式输出 | 解耦管道 | 多种模式 |
| 运行时 | 无 GIL，真正并行 | asyncio |
| 部署 | Rust 能跑的任何地方 | 需要 Python 运行时 |
| 可观测性 | 内置 Trace + Mutation Log | LangSmith（云服务） |

LeLLM 的图引擎**对 LLM 零依赖**，可观测性**内置** —— 无需付费云服务。

---

## 路线图

| 版本 | 范围 | 状态 |
|---|---|---|
| **v0.1** | Provider 抽象、流式执行、工具执行 | ✅ 已完成 |
| **v0.2** | Graph 编排、Provider 扩展 API | ✅ 已完成 |
| **v0.3** | Agent graph runtime —— ReAct 循环、Barrier | ✅ 已完成 |
| **v0.4** | ReAct = 内部图、类型化状态、检查点、Trace | ✅ 已完成 |
| **v0.5** | Graph 是 Runtime、Agent 是 DSL、执行 Session | 🔜 进行中 |
| **v0.6** | 分布式执行、可视化可观测 | 计划中 |

---

## 了解更多

- [产品蓝图](./docs/BLUEPRINT.md) —— 产品蓝图与核心 API 契约
- [设计文档](./docs/DESIGN.md) —— 关键设计决策
- [LangGraph vs LeLLM Graph](./docs/langgraph-vs-lellm-graph.md) —— 架构对比

## 许可证

MIT
