# LeLLM

LeLLM 传递快乐。人嘛，最重要的就是开心。

Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，以及快速构建常用应用的高层接口
- 标准化消息内容格式
- 统一的 provider 适配层（OpenAI、Anthropic 等）
- 低层编排能力 —— function call、agent loop、tool use、MCP
- 流式输出、多轮对话、工具调用

## 安装

```toml
[dependencies]
lellm-core = "0.1"
lellm-provider = "0.1"
```

## 快速开始

### 初始化 Provider

```rust
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;

let provider = GenericProvider::new(
    OpenAICompatAdapter::openai(),
    ProviderConfig {
        base_url: "https://api.openai.com/v1".into(),
        api_key: std::env::var("OPENAI_API_KEY").unwrap(),
        model: "gpt-4o".into(),
        timeout_secs: 120,
    },
);
```

### 单条消息调用

```rust
use lellm_core::{ChatRequest, ContentBlock};
use lellm_provider::LlmProvider;

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
use lellm_core::{ChatRequest, Message, text_block};

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
use lellm_provider::{LlmProvider, ProviderEvent};

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

## 支持的 Provider

| Provider | Adapter | 说明 |
|----------|---------|------|
| OpenAI | `OpenAICompatAdapter::openai()` | GPT-4o, GPT-5.4 等 |
| Anthropic | `AnthropicAdapter` | Claude Sonnet, Opus 等 |
| NVIDIA | `OpenAICompatAdapter::nvidia()` | OpenAI 兼容接口 |
| DeepSeek | `OpenAICompatAdapter::deepseek()` | OpenAI 兼容接口 |
| vLLM | `OpenAICompatAdapter::vllm()` | OpenAI 兼容接口 |
| LLaMA | `OpenAICompatAdapter::llama()` | OpenAI 兼容接口 |

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

## 项目结构

```
lellm/
├── lellm-core/          # 核心类型（Message, ChatRequest, LlmError 等）
├── lellm-provider/      # Provider 适配层（OpenAI, Anthropic, ...）
├── lellm-agent/         # Agent 编排层（进行中）
└── lellm-macros/        # 宏（进行中）
```
