# LeLLM

LeLLM 传递快乐。人嘛，最重要的就是开心。

Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，以及快速构建常用应用的高层接口
- 标准化消息内容格式
- 统一的 provider 适配层（OpenAI、Anthropic 等）
- 低层编排能力 —— function call、agent loop、tool use、MCP
- 流式输出、多轮对话、工具调用

## 安装

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

Feature 依赖图：

```
core          — 仅 lellm-core（零运行时依赖）
provider      — core + lellm-provider
agent         — core + provider + lellm-agent
macros        — lellm-macros
full          — core + provider + agent + macros
```

## 快速开始

### 初始化 Provider

**方式一：从环境变量自动加载（推荐）**

```rust
use lellm::provider::CodecProvider;
use lellm::provider::OpenAICompatCodec;

// 自动读取 OPENAI_BASE_URL（可选）+ OPENAI_API_KEY（必需）
let provider = CodecProvider::from_env(OpenAICompatCodec::openai())?;
```

**方式二：自定义超时等配置**

```rust
use lellm::provider::{CodecProvider, ProviderConfig};
use lellm::provider::OpenAICompatCodec;

let codec = OpenAICompatCodec::openai();
let provider = CodecProvider::new(
    codec,
    ProviderConfig::from_codec(&codec)?
        .with_timeout(std::time::Duration::from_secs(60))
        .with_idle_timeout(std::time::Duration::from_secs(30)),
);
```

**环境变量约定：** 前缀 = `provider_id().to_ascii_uppercase()`

| Provider | URL 变量 | Key 变量 | 默认 URL |
|----------|----------|----------|----------|
| openai | `OPENAI_BASE_URL` | `OPENAI_API_KEY` | `https://api.openai.com/v1` |
| deepseek | `DEEPSEEK_BASE_URL` | `DEEPSEEK_API_KEY` | `https://api.deepseek.com/v1` |
| nvidia | `NVIDIA_BASE_URL` | `NVIDIA_API_KEY` | `https://integrate.api.nvidia.com/v1` |
| anthropic | `ANTHROPIC_BASE_URL` | `ANTHROPIC_API_KEY` | `https://api.anthropic.com` |

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
println!(
    "Token: prompt={}, completion={}, total={}",
    response.usage.prompt_tokens,
    response.usage.completion_tokens,
    response.usage.total_tokens,
);
```

### 多轮对话

```rust
use lellm::core::{ChatRequest, Message, text_block};

let messages: Vec<Message> = vec![
    Message::System {
        content: text_block("你是一个将英语翻译成法语的助手。".into()),
    },
    Message::User {
        content: text_block("翻译: I like programming.".into()),
    },
    Message::Assistant {
        content: text_block("J'aime la programmation.".into()),
    },
    Message::User {
        content: text_block("翻译: I like building apps.".into()),
    },
];

let request = ChatRequest {
    messages,
    ..Default::default()
};

let response = provider.call(&request).await?;
```

### 流式输出

```rust
use futures_util::StreamExt;
use lellm::provider::{LlmProvider, ProviderEvent};

let request = ChatRequest::user_prompt("用三句话介绍 Rust。".into());
let mut stream = provider.stream(&request).await?;

while let Some(event) = stream.next().await {
    match event? {
        ProviderEvent::Start { model } => {
            eprintln!("[开始] model={}", model);
        }
        ProviderEvent::Token { token } => {
            print!("{}", token);
            std::io::Write::flush(&mut std::io::stdout()).ok();
        }
        ProviderEvent::Done { tool_calls, usage } => {
            println!();
            if let Some(u) = usage {
                eprintln!("[完成] tokens={}", u.total_tokens);
            }
        }
    }
}
```

### Agent Loop — 工具调用闭环

```rust
use lellm::agent::{AgentBuilder, ToolRegistration, StopReason};

// 注册工具
let search_tool = ToolRegistration::new(
    "search",
    "搜索互联网信息",
    search_fn,  // 你的异步工具函数
);

// 解析模型（通过 Router + ProviderRegistry）
let resolved = /* ... */;

// 构建 Agent
let agent = AgentBuilder::new(resolved)
    .tool(search_tool)
    .max_iterations(10)  // 最多 10 次 Provider 调用
    .build();

// 执行 Agent Loop
let result = agent.execute(messages).await?;

match result.stop_reason {
    StopReason::Complete => {
        println!("Agent 完成，共 {} 轮", result.iterations);
    }
    StopReason::MaxIterationsReached => {
        eprintln!("达到最大轮次 ({})", result.iterations);
    }
    _ => {
        eprintln!("Agent 因 {:?} 停止", result.stop_reason);
    }
}
```

**`max_iterations` 语义：**

`max_iterations` = **Provider 调用次数上限**（即最多发起多少次 LLM 请求）。

每次迭代 = 一次 Provider 调用 + 可选的工具执行。达到上限后，无论 Agent 是否还有未完成的 tool_calls，都会返回 `StopReason::MaxIterationsReached`。

```
max_iterations = 3 的执行流程：

  User
    ↓
  Provider #1  ← iteration 1
    ↓
  Assistant(tool_calls)
    ↓
  Tool Execute
    ↓
  ToolResult
    ↓
  Provider #2  ← iteration 2
    ↓
  Assistant(tool_calls)
    ↓
  Tool Execute
    ↓
  ToolResult
    ↓
  Provider #3  ← iteration 3
    ↓
  Assistant(text)   ← 无 tool_calls，自然结束
    ↓
  STOP(Complete)
```

**默认值：** `max_iterations = 10`（可通过 `set_max_iterations()` 调整）。

**为什么用 Provider 调用次数而非「完整轮次」：**
- 资源可控 — 直接对应 API 调用次数、token 消耗、延迟估算
- 语义简单 — 一次迭代 = 一次 Provider 调用，无歧义
- 便于 Rate Limit 规划

## 支持的 Provider

| Provider | Codec | 说明 |
|----------|-------|------|
| OpenAI | `OpenAICompatCodec::openai()` | GPT-4o, GPT-5.4 等 |
| Anthropic | `AnthropicCodec` | Claude Sonnet, Opus 等 |
| NVIDIA | `OpenAICompatCodec::nvidia()` | OpenAI 兼容接口 |
| DeepSeek | `OpenAICompatCodec::deepseek()` | OpenAI 兼容接口 |
| vLLM | `OpenAICompatCodec::vllm()` | OpenAI 兼容接口 |
| LLaMA | `OpenAICompatCodec::llama()` | OpenAI 兼容接口 |

## 运行示例

```bash
# 单条消息
OPENAI_BASE_URL=https://api.openai.com \
OPENAI_API_KEY=sk-xxx \
cargo run -p lellm-provider --example quickstart

# 多轮对话
OPENAI_BASE_URL=https://api.openai.com \
OPENAI_API_KEY=sk-xxx \
ANTHROPIC_API_KEY=sk-ant-xxx \
cargo run -p lellm-provider --example conversation

# 流式输出
OPENAI_BASE_URL=https://api.openai.com \
OPENAI_API_KEY=sk-xxx \
cargo run -p lellm-provider --example streaming
```

## 架构设计

### Provider 三层架构

```
用户 → LlmProvider (public API)
       → CodecProvider<C> (框架内部)
          → ProviderExtension 三权分立 (生态扩展 SPI)
              ├── ChatCodec (协议编解码)
              ├── ModelCapabilities (能力矩阵)
              └── ProviderMeta (连接元数据)
```

**职责切分：**

| 职责 | Codec | stream/ 模块 | CodecProvider |
|------|-------|-------------|---------------|
| Endpoint 路径 | ✅ | ❌ | ❌ |
| JSON 请求体格式 | ✅ | ❌ | ❌ |
| 协议特定 Header | ✅ | ❌ | ❌ |
| SseFrame → StreamChunk 解析 | ✅ | ❌ | ❌ |
| SseParser (行缓冲 + SseFrame) | ❌ | ✅ | ❌ |
| ToolCallAccumulator | ❌ | ✅ | ❌ |
| process_stream (管道编排) | ❌ | ✅ | ❌ |
| EventSink / StreamEvent | ❌ | ✅ (trait) | ❌ |
| HTTP Client | ❌ | ❌ | ✅ |
| base_url / api_key / timeout | ❌ | ❌ | ✅ |

### 流式传输层解耦

`stream/` 模块完全不知道 `reqwest`、`tokio channel` 等传输细节。

```
┌─────────────────────────────────────┐
│ CodecProvider (base.rs)             │
│ 知道: reqwest, tokio channel        │
│ 职责: HTTP 发送, ChannelSink 桥接    │
└─────────────────────────────────────┘
                  ↓
┌─────────────────────────────────────┐
│ process_stream (stream_processor)   │
│ 签名: Stream<Item=Result<Bytes>>    │
│       EventSink (fn emit)           │
│ 不知道: reqwest, tokio, ProviderEvent│
└─────────────────────────────────────┘
                  ↓
┌─────────────────────────────────────┐
│ SseParser + Codec + Accumulator     │
│ 纯逻辑，无 IO                       │
└─────────────────────────────────────┘
```

**测试价值：** 无需 mock HTTP，直接构造 Stream 即可测试 SSE 解析管道：

```rust
let stream = futures_util::stream::iter(vec![
    Ok(Bytes::from("data: {\"text\": \"hel\"}\n\n")),
    Ok(Bytes::from("data: {\"text\": \"lo\"}\n\n")),
]);
process_stream(&mut mock_sink, &codec, "test".into(), stream).await;
```

## 项目结构

```
lellm/
├── lellm/               # Facade 统一入口
├── lellm-core/          # 协议（Message, ChatRequest, LlmError 等）
├── lellm-provider/      # Provider 适配层（OpenAI, Anthropic, ...）
│   ├── providers/       # Codec + CodecProvider
│   │   ├── base.rs      # CodecProvider, ProviderConfig, AuthConfig
│   │   ├── codec.rs     # ChatCodec, ModelCapabilities, ProviderMeta, ProviderExtension
│   │   ├── stream/      # 传输层解耦的流式处理管道
│   │   │   ├── sse_parser.rs
│   │   │   ├── stream_processor.rs
│   │   │   └── tool_call_accumulator.rs
│   │   ├── anthropic.rs
│   │   └── openai_compat.rs
│   └── router.rs        # ModelRouter + ProviderRegistry
├── lellm-agent/         # Agent Runtime（ToolUseLoop, Executor, ...）
└── lellm-macros/        # Derive 宏
```

## 详细设计

- [BLUEPRINT.md](./docs/BLUEPRINT.md) — 产品蓝图与核心 API 契约
- [DESIGN.md](./docs/DESIGN.md) — 关键设计决策的为什么与如何实现
