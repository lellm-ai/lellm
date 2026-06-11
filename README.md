# LeLLM

> LeLLM 传递快乐。人嘛，最重要的就是开心。

Type-safe LLM application framework for Rust.

Build production AI systems in Rust with predictable runtime behavior, provider abstraction, streaming pipelines, and agent execution — without rebuilding the same infrastructure every time.

[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.1.1-green)](./CHANGELOG.md)

```bash
cargo add lellm
```

```rust
let response = agent
    .prompt("Summarize this report")
    .execute()
    .await?;
```

---

## Why LeLLM

Most AI frameworks optimize for speed of prototyping.

**LeLLM optimizes for production reliability.**

When building real AI systems, the hard parts are rarely calling an API. They are:

- Provider differences (OpenAI / Anthropic / Gemini / OpenRouter)
- Streaming and partial failures
- Tool execution and retries
- Token budgets and runaway loops
- Context growth and memory pressure
- Runtime observability

LeLLM provides these as composable runtime primitives.

---

## Who LeLLM Is For

LeLLM is designed for engineers building AI systems in Rust.

### Good fit

- Backend and infrastructure engineers
- Agent and workflow platform builders
- Teams requiring deterministic runtime behavior
- Edge / embedded / low-resource deployments
- Rust users who want compile-time guarantees

**Typical workloads:**

- AI APIs and gateways
- Internal copilots
- Agent runtimes
- Multi-provider orchestration
- Real-time streaming applications
- Long-running autonomous workflows

### Probably not for you

- Notebook-first experimentation
- Prompt engineering only
- No-code workflows
- Simple one-off API calls
- Learning Rust through AI

If your application is `HTTP → LLM → return`, `reqwest` + `serde` is probably enough.

LeLLM starts paying off when orchestration complexity appears.

---

## What Problems LeLLM Solves

| Without LeLLM | With LeLLM |
|---|---|
| Provider integration → repeated HTTP/SSE work | Provider abstraction |
| Tool orchestration → custom control loops | Agent loop execution |
| Retry & fallback → edge cases everywhere | Retry & fallback |
| Context management → memory growth | Context compaction |
| Budget enforcement → difficult to retrofit | Token budget protection |

**Pipeline:** `Provider → Agent → Tool → Stream → Runtime`

**Included:**

- Provider abstraction
- Streaming pipeline
- Agent loop execution
- Tool system
- Retry & fallback
- Token budget protection
- Context compaction
- Typed errors

---

## Design Principles

LeLLM intentionally prefers explicitness over magic.

### Type Safety First

Invalid states should fail at compile time whenever possible.

### Runtime Control Over Automation

Retries, streaming, budgets, and memory policies remain observable and configurable.

### Composition Over Framework Lock-In

LeLLM components can run independently. Use only what you need.

```
lellm-core
    ↓
lellm-provider
    ↓
lellm-agent
```

### Provider Protocol ≠ Runtime Logic

Provider integration is separated into three concerns:

```
ChatCodec + ModelCapabilities + ProviderMeta
```

This separation allows protocol evolution without rewriting execution logic.

---

## Where LeLLM Fits

| Use Case | Fit |
|---|---|
| AI API backend | Excellent |
| Agent runtime | Excellent |
| Multi-provider routing | Excellent |
| Streaming applications | Excellent |
| Edge deployment | Strong |
| Rapid notebook iteration | Weak |
| Visual workflow builders | Not focus |

---

## Comparison

| | LeLLM | Python Agent Frameworks |
|---|---|---|
| Language | Rust | Python |
| Type Safety | Compile-time | Runtime |
| Runtime Control | High | Medium |
| Streaming | Native | Framework dependent |
| Provider Abstraction | Built-in | Varies |
| Budget Enforcement | Built-in | Usually external |
| Context Management | Built-in | Partial |
| Ecosystem | Early | Mature |

LeLLM is not trying to replace Python. It exists for teams that already chose Rust.

---

## Quick Start

### Install

All features are opt-in (`default = []`), keeping `lellm-core` zero-runtime-dependency:

```toml
[dependencies]
# Protocol types only (zero runtime dependencies)
lellm = { version = "0.1", features = ["core"] }

# Protocol + Provider adapter layer
lellm = { version = "0.1", features = ["provider"] }

# Protocol + Provider + Agent runtime
lellm = { version = "0.1", features = ["agent"] }

# Everything
lellm = { version = "0.1", features = ["full"] }
```

### Initialize a Provider

```rust
use lellm::provider::CodecProvider;
use lellm::provider::OpenAICompatCodec;

// Auto-load from OPENAI_BASE_URL + OPENAI_API_KEY
let provider = CodecProvider::from_env(OpenAICompatCodec::openai())?;
```

### Single Message Call

```rust
use lellm::core::{ChatRequest, ContentBlock};
use lellm::provider::LlmProvider;

let request = ChatRequest::user_prompt("Why do parrots have colorful feathers?".into())
    .with_temperature(0.7);

let response = provider.call(&request).await?;
for block in &response.content {
    if let ContentBlock::Text(t) = block {
        print!("{}", t.text);
    }
}
```

### Agent Loop with Tools

```rust
use lellm::agent::{AgentBuilder, ToolRegistration, StopReason};

let agent = AgentBuilder::new(resolved)
    .tool(ToolRegistration::new("search", "Search the internet", search_fn))
    .max_iterations(10)
    .build();

let result = agent.execute(messages).await?;

match result.stop_reason {
    StopReason::Complete => println!("Done in {} iterations", result.iterations),
    StopReason::MaxIterationsReached => eprintln!("Max iterations reached"),
    _ => eprintln!("Stopped: {:?}", result.stop_reason),
}
```

### Streaming Output

```rust
use futures_util::StreamExt;
use lellm::provider::{LlmProvider, ProviderEvent};

let mut stream = provider.stream(&request).await?;

while let Some(event) = stream.next().await {
    match event? {
        ProviderEvent::Token { token } => print!("{}", token),
        ProviderEvent::Done { usage, .. } => {
            if let Some(u) = usage {
                eprintln!("\nTokens: {}", u.total_tokens);
            }
        }
        _ => {}
    }
}
```

---

## Architecture

### Provider Three-Way Split

```
User → LlmProvider (public API)
       → CodecProvider<C> (framework internal)
          → ProviderExtension (ecosystem SPI)
              ├── ChatCodec (protocol encoding/decoding)
              ├── ModelCapabilities (capability matrix)
              └── ProviderMeta (connection metadata)
```

### Decoupled Streaming Pipeline

`stream/` knows nothing about `reqwest` or `tokio channels`:

```
CodecProvider (HTTP, channels)
       ↓
process_stream (Stream<Item=Result<Bytes>>, EventSink)
       ↓
SseParser + Codec + Accumulator (pure logic, no IO)
```

### Crate Layout

```
lellm/
├── lellm/               # Facade — unified entry point
├── lellm-core/          # Protocol (Message, ChatRequest, LlmError, ...)
├── lellm-provider/      # Provider adapter layer
├── lellm-agent/         # Agent runtime (ToolUseLoop, Executor, ...)
└── lellm-macros/        # Derive + attribute macros
```

---

## Roadmap

| Version | Scope | Status |
|---|---|---|
| **v0.1** | Provider abstraction, streaming, tool execution, budget enforcement, context compaction | ✅ Done |
| **v0.2** | Graph orchestration, provider extension API, memory architecture, more provider compatibility | 🚧 In progress |
| **v0.3+** | Distributed execution, visual observability, multi-agent coordination | 🔜 Planned |

---

## Philosophy

Build AI systems the same way we build databases, gateways, and distributed services:

**explicit, observable, type-safe.**

---

## Links

- [Blueprint](./docs/BLUEPRINT.md) — Product blueprint and API contracts
- [Design Doc](./docs/DESIGN.md) — Key design decisions and rationale

## License

MIT
