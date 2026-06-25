# LeLLM

> LeLLM 传递快乐。人嘛，最重要的就是开心。

Rust 类型安全的 LLM 应用框架。

用 Rust 构建生产级 AI 系统——可预测的运行时行为、Provider 抽象、流式管道、Agent 执行、Graph 编排。

[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.4.0-green)](./CHANGELOG.md)

```bash
cargo add lellm
```

```rust
use lellm::agent::AgentBuilder;
use lellm::core::Message;

let agent = AgentBuilder::new(model)
    .system_prompt("你是一个有用的助手。".into())
    .tool(weather_tool)
    .max_iterations(10)
    .build();

let result = agent.execute(vec![Message::user_text("今天上海天气如何？")]).await?;
```

---

## 为什么需要 LeLLM

大多数 AI 框架优化的是原型速度。**LeLLM 优化的是生产可靠性。**

构建真实的 AI 系统时，困难的部分很少是调 API。真正棘手的是：

- Provider 差异（OpenAI / Anthropic / Gemini / OpenRouter）
- 流式输出与部分失败
- 工具执行与重试
- Token 预算与失控循环
- 上下文增长与内存压力
- 运行时可观测性

LeLLM 将这些能力提供为可组合的运行时原语。

---

## 为谁而建

LeLLM 为用 Rust 构建 AI 系统的工程师设计。

### 适合你

- 后端与基础设施工程师
- Agent 与工作流平台构建者
- 需要确定性运行时行为的团队
- 边缘 / 嵌入式 / 低资源部署
- 想要编译期保证的 Rust 用户

**典型负载：**

- AI API 与网关
- 内部 Copilot
- Agent 运行时
- 多 Provider 编排
- 实时流式应用
- 长周期自主工作流

### 可能不适合你

- Notebook 优先的实验
- 只做 Prompt 工程
- 无代码工作流
- 简单的一次性 API 调用
- 想通过 AI 学 Rust

如果你的应用只是 `HTTP → LLM → return`，`reqwest` + `serde` 大概率就够了。

当编排复杂度出现时，LeLLM 才开始发挥价值。

---

## 设计原则

### 类型安全优先

无效状态尽可能在编译期报错。

### 显式优于魔法

重试、流式、预算、内存策略保持可观测、可配置。

### 组合优于框架锁定

组件可独立使用，按需组合：

```
lellm-core → lellm-provider → lellm-agent → lellm-graph
```

### Provider 协议 ≠ 运行时逻辑

Provider 集成分离为三个关注点：`ChatCodec + ModelCapabilities + ProviderMeta`

---

## 快速开始

### 安装

默认开启 `provider`（core + provider 适配层）。其他 feature 按需开启：

```toml
[dependencies]
# 默认：core + provider
lellm = "0.4"

# Agent 运行时
lellm = { version = "0.4", features = ["agent"] }

# Graph 编排
lellm = { version = "0.4", features = ["graph"] }

# 全部启用
lellm = { version = "0.4", features = ["full"] }
```

### 初始化 Provider

```rust
use lellm::provider::{CodecProvider, OpenAICompatCodec};

// 自动读取 OPENAI_BASE_URL + OPENAI_API_KEY
let provider = CodecProvider::load(OpenAICompatCodec::openai())?;
```

**支持的 Provider：**

| Provider | Codec |
|---|---|
| OpenAI | `OpenAICompatCodec::openai()` |
| Anthropic | `AnthropicCodec` |
| Google | `GoogleCodec` |
| DeepSeek | `OpenAICompatCodec::deepseek()` |
| NVIDIA | `OpenAICompatCodec::nvidia()` |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` / `::llama()` |

### 单条消息调用

```rust
use lellm::core::{ChatRequest, ContentBlock};
use lellm::provider::LlmProvider;

let request = ChatRequest::user_prompt("为什么鹦鹉有五颜六色的羽毛？".into())
    .with_temperature(0.7);

let response = provider.call(&request).await?;
for block in &response.content {
    if let ContentBlock::Text(t) = block {
        print!("{}", t.text);
    }
}
```

### Agent 循环与工具调用

```rust
use lellm::agent::{AgentBuilder, StopReason};
use lellm::core::Message;
use lellm::provider::ResolvedModel;
use std::sync::Arc;

let model = ResolvedModel {
    provider: Arc::new(provider),
    model: "gpt-4o".into(),
    context_window: None,
};

let agent = AgentBuilder::new(model)
    .system_prompt("你是一个有用的助手。".into())
    .tool(search_tool)
    .max_iterations(10)
    .max_output_tokens(8000)
    .build();

let result = agent.execute(vec![Message::user_text("今天上海天气如何？")]).await?;

match result.stop_reason {
    StopReason::Complete => println!("完成，共 {} 轮", result.iterations),
    StopReason::MaxIterationsReached => eprintln!("达到最大轮次"),
    _ => eprintln!("停止原因: {:?}", result.stop_reason),
}
```

### 流式输出

```rust
use futures_util::StreamExt;
use lellm::provider::{LlmProvider, ProviderEvent};

let mut stream = provider.stream(&request).await?;

while let Some(event) = stream.next().await {
    match event? {
        ProviderEvent::Token { token } => print!("{}", token),
        ProviderEvent::ResponseComplete { usage, .. } => {
            if let Some(u) = usage {
                eprintln!("\nToken: {}", u.total_tokens);
            }
        }
        _ => {}
    }
}
```

### 工具定义

**方式一：`#[tool]` 函数宏（推荐，95% 场景）**

```rust
use lellm::core::ToolResult;
use lellm::derive::tool;

#[tool(name = "get_weather", description = "获取指定城市的天气")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// 注册：
builder.tool(get_weather_tool());
```

**方式二：`#[derive(Tool)]` struct 宏**

```rust
use lellm::derive::Tool;
use lellm::agent::ToolArgs;
use lellm::core::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "get_weather", description = "获取指定城市的天气")]
struct GetWeatherArgs {
    /// 城市名称
    city: String,
}

// 注册 —— 闭包接收反序列化后的 struct：
let tool = GetWeatherArgs::safe(|args| async move {
    Ok(serde_json::json!({"city": args.city, "temp": 25}))
});
```

---

## 架构

### Crate 布局

```
lellm/
├── lellm/               # 门面 crate —— 统一入口
├── lellm-core/          # 协议对象（Message, ChatRequest, LlmError 等）
├── lellm-provider/      # Provider 适配层
├── lellm-agent/         # Agent 运行时（ToolUseLoop, Executor 等）
├── lellm-graph/         # Graph 编排（Node, Edge, Barrier, Multi-Agent）
├── lellm-derive/        # 派生宏 + 属性宏
└── lellm-mcp/           # MCP（Model Context Protocol）客户端/服务端
```

### Provider 三权分立

```
用户 → LlmProvider（公开 API）
       → CodecProvider<C>（框架内部）
          → ProviderExtension（生态扩展 SPI）
              ├── ChatCodec（协议编解码）
              ├── ModelCapabilities（能力矩阵）
              └── ProviderMeta（连接元数据）
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
| **v0.1** | Provider 抽象、流式执行、工具执行、预算控制、上下文压缩 | ✅ 已完成 |
| **v0.2** | Graph 编排、Provider 扩展 API、内存架构、更多 Provider 兼容 | ✅ 已完成 |
| **v0.3** | Agent graph runtime — ReAct loop, barriers, multi-agent | ✅ 已完成 |
| **v0.4** | ReAct Graph mode, post-agent hooks, stop config export | ✅ 已完成 |
| **v0.5+** | 分布式执行、可视化可观测 | 🔜 计划中 |

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
