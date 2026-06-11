# LeLLM

> LeLLM 传递快乐。人嘛，最重要的就是开心。

Rust 类型安全的 LLM 应用框架。

用 Rust 构建生产级 AI 系统——可预测的运行时行为、Provider 抽象、流式管道、Agent 执行，无需每次重复造轮子。

[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.1.1-green)](./CHANGELOG.md)

```bash
cargo add lellm
```

```rust
use lellm::agent::AgentBuilder;
use lellm::core::{Message, text_block};

let agent = AgentBuilder::new(model)
    .system_prompt("你是一个有用的助手。".into())
    .tool(weather_tool)
    .max_iterations(10)
    .build();

let result = agent
    .execute(vec![Message::User {
        content: text_block("今天上海天气如何？".into()),
    }])
    .await?;
```

---

## 为什么需要 LeLLM

大多数 AI 框架优化的是原型速度。

**LeLLM 优化的是生产可靠性。**

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

## LeLLM 解决什么问题

| 没有 LeLLM | 有了 LeLLM |
|---|---|
| Provider 集成 → 重复的 HTTP/SSE 工作 | Provider 抽象 |
| 工具编排 → 手写控制循环 | Agent 循环执行 |
| 重试与降级 → 到处都是边界情况 | 重试与降级 |
| 上下文管理 → 内存增长 | 上下文压缩 |
| 预算控制 → 事后补救困难 | Token 预算保护 |

**管道：** `Provider → Agent → Tool → Stream → Runtime`

**开箱即用：**

- Provider 抽象
- 流式管道
- Agent 循环执行
- 工具系统
- 重试与降级
- Token 预算保护
- 上下文压缩
- 类型化错误

---

## 设计原则

LeLLM 有意选择显式，而非魔法。

### 类型安全优先

无效状态尽可能在编译期报错。

### 运行时控制优于自动化

重试、流式、预算、内存策略保持可观测、可配置。

### 组合优于框架锁定

LeLLM 组件可独立运行。按需使用：

```
lellm-core
    ↓
lellm-provider
    ↓
lellm-agent
```

### Provider 协议 ≠ 运行时逻辑

Provider 集成分离为三个关注点：

```
ChatCodec + ModelCapabilities + ProviderMeta
```

这种分离允许协议演进，无需重写执行逻辑。

---

## LeLLM 适用场景

| 场景 | 匹配度 |
|---|---|
| AI API 后端 | 优秀 |
| Agent 运行时 | 优秀 |
| 多 Provider 路由 | 优秀 |
| 流式应用 | 优秀 |
| 边缘部署 | 强 |
| Notebook 快速迭代 | 弱 |
| 可视化工作流构建器 | 非重点 |

---

## 对比

| | LeLLM | Python Agent 框架 |
|---|---|---|
| 语言 | Rust | Python |
| 类型安全 | 编译期 | 运行时 |
| 运行时控制 | 高 | 中 |
| 流式输出 | 原生 | 依赖框架 |
| Provider 抽象 | 内置 | 因框架而异 |
| 预算控制 | 内置 | 通常需外部实现 |
| 上下文管理 | 内置 | 部分 |
| 生态 | 早期 | 成熟 |

LeLLM 不试图替代 Python。它服务于已经选择了 Rust 的团队。

---

## 快速开始

### 安装

所有 feature 均需显式开启（`default = []`），保证 `lellm-core` 零运行时依赖：

```toml
[dependencies]
# 仅协议对象（零运行时依赖）
lellm = { version = "0.1", features = ["core"] }

# 协议 + Provider 适配层
lellm = { version = "0.1", features = ["provider"] }

# 协议 + Provider + Agent 运行时
lellm = { version = "0.1", features = ["agent"] }

# 全部启用
lellm = { version = "0.1", features = ["full"] }
```

### 初始化 Provider

```rust
use lellm::provider::{CodecProvider, OpenAICompatCodec};

// 自动读取 OPENAI_BASE_URL + OPENAI_API_KEY
let provider = CodecProvider::from_env(OpenAICompatCodec::openai())?;
```

**通过 OpenRouter**（聚合网关）：

```rust
use lellm::provider::{openrouter, OpenAICompatCodec, AnthropicCodec};

// 从 OPENROUTER_API_KEY 加载
let provider = openrouter(OpenAICompatCodec::openai())?;

// 换协议只需换 Codec
let anthropic_via_openrouter = openrouter(AnthropicCodec)?;
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
use lellm::core::{Message, text_block};
use lellm::provider::ResolvedModel;
use std::sync::Arc;

// 解析模型
let model = ResolvedModel {
    provider: Arc::new(provider),
    model: "gpt-4o".into(),
    context_window: None,
};

// 构建 Agent
let agent = AgentBuilder::new(model)
    .system_prompt("你是一个有用的助手。".into())
    .tool(search_tool)
    .max_iterations(10)
    .max_output_tokens(8000)
    .build();

// 执行
let result = agent
    .execute(vec![Message::User {
        content: text_block("今天上海天气如何？".into()),
    }])
    .await?;

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

### 工具定义与宏

```rust
use lellm::agent::{ToolArgs, ToolRegistration};
use lellm::macros::Tool;
use lellm::core::{ToolError, ToolErrorKind};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(
    name = "get_weather",
    description = "获取指定城市的天气"
)]
struct GetWeatherArgs {
    city: String,
}

// 使用 .safe() 注册 —— 错误被捕获并返回为 ToolError
let tool = ToolRegistration::safe(
    GetWeatherArgs::tool_definition(),
    |args| async move {
        let city = args.get("city").unwrap().as_str().unwrap().to_string();
        // ... 工具逻辑 ...
        Ok(format!("{} 的天气", city))
    },
);
```

---

## 架构设计

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

### Crate 布局

```
lellm/
├── lellm/               # 门面 crate —— 统一入口
├── lellm-core/          # 协议对象（Message, ChatRequest, LlmError 等）
├── lellm-provider/      # Provider 适配层
├── lellm-agent/         # Agent 运行时（ToolUseLoop, Executor 等）
└── lellm-macros/        # 派生宏 + 属性宏
```

---

## 路线图

| 版本 | 范围 | 状态 |
|---|---|---|
| **v0.1** | Provider 抽象、流式执行、工具执行、预算控制、上下文压缩 | ✅ 已完成 |
| **v0.2** | Graph 编排、Provider 扩展 API、内存架构、更多 Provider 兼容 | 🚧 进行中 |
| **v0.3+** | 分布式执行、可视化可观测、多 Agent 协调 | 🔜 计划中 |

---

## 理念

像构建数据库、网关、分布式服务一样构建 AI 系统：

**显式、可观测、类型安全。**

---

## 相关链接

- [产品蓝图](./docs/BLUEPRINT.md) —— 产品蓝图与核心 API 契约
- [设计文档](./docs/DESIGN.md) —— 关键设计决策的为什么与如何实现

## 许可证

MIT
