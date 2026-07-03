# LeLLM
English | [中文](./README_zh.md)

> LeLLM spreads joy. The most important thing in life is to be happy.

Type-safe LLM application framework for Rust.

Build production AI systems in Rust with predictable runtime behavior, provider abstraction, streaming pipelines, agent execution, and graph orchestration.

[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.4.6-green)](./CHANGELOG.md)

```bash
cargo add lellm
```

```rust
use lellm::agent::AgentBuilder;
use lellm::core::Message;
use lellm::provider::ResolvedModel;
use std::sync::Arc;

let model = ResolvedModel {
    provider: Arc::new(provider),
    model: "gpt-4o".into(),
    context_window: None,
};

let loop_ = AgentBuilder::new(model)
    .system("You are a helpful assistant.")
    .tool(weather_tool)
    .max_iterations(10)
    .compile();

let result = loop_.invoke(vec![Message::user_text("What's the weather in Shanghai?")]).await?;
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

## Design Principles

### Type Safety First

Invalid states should fail at compile time whenever possible.

### Explicit Over Magic

Retries, streaming, budgets, and memory policies remain observable and configurable.

### Composition Over Framework Lock-In

Diamond architecture — `lellm-graph` and `lellm-provider` are peer layers, both built on `lellm-core`.
`lellm-agent` sits on top, composing both:

```
              lellm-core (protocol types)
             /                \
            /                  \
  lellm-provider         lellm-graph
  (LLM adapters)         (workflow engine)
            \                  /
             \                /
              lellm-agent (ReAct loop = internal graph)
```

- **lellm-core** — Zero-runtime protocol types (`Message`, `ChatRequest`, `LlmError`). Standalone.
- **lellm-provider** — Provider adapters only. No graph, no agent. Standalone with core.
- **lellm-graph** — Generic workflow engine (nodes, edges, barriers, parallel, checkpoints). No LLM dependency. Standalone with core.
- **lellm-agent** — Composes provider + graph. ReAct loop is an internal graph (`LLMNode → ToolNode → …`).

### Provider Protocol ≠ Runtime Logic

Provider integration is separated into three concerns: `ChatCodec + ModelCapabilities + ProviderMeta`

---

## Quick Start

### Install

Default feature: `provider` (includes core + provider adapters). Opt-in for more:

```toml
[dependencies]
# Default: core + provider adapters (call LLMs directly)
lellm = "0.4"

# Graph orchestration only (workflow engine, no LLM dependency)
lellm = { version = "0.4", features = ["graph"] }

# Agent runtime (includes core + graph + provider + agent)
lellm = { version = "0.4", features = ["agent"] }

# Everything (graph + provider + agent + mcp + derive)
lellm = { version = "0.4", features = ["full"] }
```

**Feature dependency matrix:**

| Feature | Includes |
|---|---|
| `core` | lellm-core |
| `provider` | core + lellm-provider |
| `graph` | core + lellm-graph |
| `agent` | core + graph + provider + lellm-agent |
| `mcp` | core + graph + lellm-mcp |
| `derive` | lellm-derive |
| `full` | graph + provider + agent + mcp + derive |

### Initialize a Provider

```rust
use lellm::provider::{CodecProvider, OpenAICompatCodec};

// Auto-load from OPENAI_BASE_URL + OPENAI_API_KEY
let provider = CodecProvider::load(OpenAICompatCodec::openai())?;
```

**Supported providers:**

| Provider | Codec |
|---|---|
| OpenAI | `OpenAICompatCodec::openai()` |
| Anthropic | `AnthropicCodec` |
| Google | `GoogleCodec` |
| DeepSeek | `OpenAICompatCodec::deepseek()` |
| NVIDIA | `OpenAICompatCodec::nvidia()` |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` / `::llama()` |

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
use lellm::agent::{AgentBuilder, StopReason};
use lellm::core::Message;
use lellm::provider::ResolvedModel;
use std::sync::Arc;

let model = ResolvedModel {
    provider: Arc::new(provider),
    model: "gpt-4o".into(),
    context_window: None,
};

let loop_ = AgentBuilder::new(model)
    .system("You are a helpful assistant.")
    .tool(search_tool)
    .max_iterations(10)
    .max_output_tokens(8000)
    .compile();

let result = loop_.invoke(vec![Message::user_text("What's the weather in Shanghai?")]).await?;

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
        ProviderEvent::ResponseComplete { usage, .. } => {
            if let Some(u) = usage {
                eprintln!("\nTokens: {}", u.total_tokens);
            }
        }
        _ => {}
    }
}
```

### Tool Definition

**Option 1: `#[tool]` function macro (recommended, 95% of cases)**

```rust
use lellm::core::ToolResult;
use lellm::derive::tool;

#[tool(name = "get_weather", description = "Get the current weather for a city")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// Register:
builder.tool(get_weather_tool());
```

**Option 2: `#[derive(Tool)]` struct macro**

```rust
use lellm::derive::Tool;
use lellm::agent::ToolArgs;
use lellm::core::ToolResult;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "get_weather", description = "Get the current weather for a city")]
struct GetWeatherArgs {
    /// City name
    city: String,
}

// Register — closure receives deserialized struct:
let tool = GetWeatherArgs::safe(|args| async move {
    Ok(serde_json::json!({"city": args.city, "temp": 25}))
});
```

---

## Architecture

### Crate Layout

```
lellm/
├── lellm/               # Facade — unified entry point
├── lellm-core/          # Protocol types (Message, ChatRequest, LlmError)
│   └── deps: serde, serde_json, schemars, thiserror
├── lellm-provider/      # Provider adapters (OpenAI, Anthropic, Google, ...)
│   └── deps: lellm-core + reqwest + tokio
├── lellm-graph/         # Generic workflow engine (NO LLM dependency)
│   └── deps: lellm-core + tokio
├── lellm-agent/         # Agent runtime (composes provider + graph)
│   └── deps: lellm-core + lellm-provider + lellm-graph
├── lellm-derive/        # Derive + attribute macros (proc-macro, no internal deps)
└── lellm-mcp/           # MCP (Model Context Protocol) client/server
    └── deps: lellm-core + optional lellm-agent
```

### Graph Orchestration

`lellm-graph` is a generic workflow engine — similar in scope to LangGraph / Temporal / Prefect.
It has **zero LLM dependency**, only depending on `lellm-core` for protocol types.

**Node Types:**

| Node | Purpose |
|---|---|
| `TaskNode` | Simple function node (`fn(&mut State) -> Result`) |
| `External` | Custom `FlowNode<S>` impl (e.g. LLMNode, AgentFlowNode) |
| `ConditionNode` | Conditional branching — routes to different targets by state |
| `BarrierNode` | Human-in-the-loop — pauses execution, awaits external decision |
| `ParallelNode` | Fan-out/fan-in — parallel branches with MergeStrategy |

**Edge Model — Three-Tier Routing:**

| Edge Type | Priority | Behavior |
|---|---|---|
| Conditional | Highest | Routes when condition function matches state |
| Normal | Middle | Default path when no condition matches |
| Fallback | Lowest | Final safety net — catches unrouted states |

**Typed State System:**

```
State (HashMap<String, Value>) — backward compatible, dynamic
AgentState (strongly-typed struct) — compile-time safe, zero serialization

Nodes emit typed Effects → NodeContext buffers → Executor applies to State.
Parallel branches clone base state, execute independently, merge via MergeStrategy.
```

**Execution Modes:**

- **Blocking** — `executor.execute(graph, state)` → `GraphResult`
- **Streaming** — `executor.execute_stream(graph, state)` → `GraphExecution` (channel-based events)
- **Inline** — `graph.run_inline(&mut ctx, max_steps)` — used by ReAct loop (no channel overhead)

**Checkpoint & Resume:**

- `CheckpointStore` trait — persist state at node boundaries
- `CheckpointPolicy` — `EveryNode` / `BarrierOnly` / `Manual`
- `executor.resume_from(store, trace_id, graph)` — restore from last checkpoint

**ReAct as Internal Graph:**

The Agent's tool-use loop is NOT a hand-rolled while loop — it builds an internal graph:

```
START → budget_check ──(ok)──→ [llm] → [post_llm_check]
         │                         │              │
      (compact) → [compactor]     │       has_tools → [tool] → budget_check (loop)
                                   │       no_tools  → [end]
                              (thinking)
```

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

---

## Roadmap

| Version | Scope | Status |
|---|---|---|
| **v0.1** | Provider abstraction, streaming, tool execution, budget enforcement, context compaction | ✅ Done |
| **v0.2** | Graph orchestration, provider extension API, memory architecture, more provider compatibility | ✅ Done |
| **v0.3** | Agent graph runtime — ReAct loop, barriers, multi-agent coordination | ✅ Done |
| **v0.4** | ReAct Graph mode, post-agent hooks, stop config export | ✅ Done |
| **v0.5+** | Distributed execution, visual observability | 🔜 Planned |

---

## Philosophy

Build AI systems the same way we build databases, gateways, and distributed services:

**explicit, observable, type-safe.**

---

## Links

- [Blueprint](./docs/BLUEPRINT.md) — Product blueprint and API contracts
- [Design Doc](./docs/DESIGN.md) — Key design decisions and rationale
- [LangGraph vs LeLLM Graph](./docs/langgraph-vs-lellm-graph.md) — Detailed architecture comparison

## License

MIT
