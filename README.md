# LeLLM

English | [中文](./README_zh.md)

> LeLLM spreads joy. The most important thing in life is to be happy.

**Production-grade LLM orchestration framework in Rust** — with compile-time type safety, zero-cost abstractions, and predictable runtime behavior.

Build reliable AI systems: agent loops, tool use, workflow graphs, checkpointing, and human-in-the-loop — all without magic.

[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Version](https://img.shields.io/badge/version-0.4.10-green)](./CHANGELOG.md)

## Quick Start

```rust
use lellm::prelude::*;

// 1. Provider — auto-loads from OPENAI_API_KEY
let provider = CodecProvider::load(OpenAICompatCodec::openai())?;

// 2. Define a tool
#[tool(name = "get_weather", description = "Get current weather for a city")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// 3. Build an agent with tool-use loop
let model = ResolvedModel {
    provider: Arc::new(provider),
    model: "gpt-4o".into(),
    context_window: None,
};

let agent = AgentBuilder::new(model)
    .system("You are a helpful assistant.")
    .tool(get_weather_tool())
    .max_iterations(10)
    .compile();

// 4. Run it
let result = agent
    .invoke(vec![Message::user_text("What's the weather in Shanghai?")])
    .await?;

match result.stop_reason {
    StopReason::Complete => println!("Done in {} iterations", result.iterations),
    StopReason::MaxIterationsReached => eprintln!("Max iterations reached"),
    _ => eprintln!("Stopped: {:?}", result.stop_reason),
}
```

**That's a complete, production-ready agent loop** — with budget enforcement, tool retry, context compaction, and streaming support. All type-safe. All explicit.

---

## Why LeLLM

Most AI frameworks optimize for speed of prototyping. **LeLLM optimizes for production reliability.**

When building real AI systems, calling an API is the easy part. The hard parts are:

| Problem | LeLLM's Answer |
|---|---|
| Provider differences (OpenAI / Anthropic / Gemini) | Unified `ChatCodec` — one API, six providers |
| Runaway agent loops | Hard `max_iterations` + token budget enforcement |
| Context overflow | Pluggable compaction strategy |
| Tool execution failures | Typed retry policy + `ParallelSafety` categories |
| Partial stream failures | Decoupled streaming pipeline — pure logic, no IO |
| State consistency | Checkpoint + Mutation Log + Trace audit trail |
| Human approval workflows | `BarrierNode` — pause, decide, resume |
| Parallel workflows | `ParallelNode` — fan-out / fan-in with merge strategy |

---

## How LeLLM Compares

| | **LeLLM** | **LangChain (Python)** | **LangGraph (Python)** | **Semantic Kernel** |
|---|---|---|---|---|
| **Language** | Rust | Python | Python | C#/Java/Python |
| **Type Safety** | Compile-time guarantees | Runtime checks | Runtime checks | Partial |
| **Graph Engine** | Built-in, zero LLM dep | External (LangGraph) | Built-in | Limited |
| **Agent Loop** | ReAct = internal graph | Hand-rolled while loop | State machine | Linear chain |
| **Checkpointing** | Built-in, typed | External | Built-in | Limited |
| **Streaming** | Decoupled pipeline | Provider-specific | Provider-specific | Provider-specific |
| **Runtime Overhead** | Minimal (no GIL) | CPython overhead | CPython overhead | .NET overhead |
| **Deploy Target** | Anywhere Rust runs | Python runtime needed | Python runtime needed | .NET runtime needed |
| **Philosophy** | Explicit, observable | Convention over config | DAG-based | SDK-style |

**LeLLM is what LangGraph would look like if designed for Rust from day one.**

---

## Who LeLLM Is For

### Built for you if you are:

- **Backend / infrastructure engineers** building AI-powered services
- **Platform teams** building agent runtimes or orchestration layers
- **Performance-sensitive** applications (edge, embedded, low-latency)
- Teams requiring **deterministic runtime behavior** and observability
- **Rust shops** wanting compile-time guarantees for AI systems

**Typical workloads:**
- AI APIs and gateways
- Internal copilots
- Agent runtimes & multi-agent orchestration
- Real-time streaming applications
- Long-running autonomous workflows

### Probably not for you if:

- You primarily experiment in Jupyter notebooks
- Your app is just `HTTP → LLM → return` (use `reqwest` + `serde`)
- You want no-code / low-code workflows
- You're learning Rust through AI projects

LeLLM starts paying off when **orchestration complexity** appears.

---

## Design Principles

### Type Safety First

Invalid states should fail at compile time. `ToolArgs` is a strongly-typed struct, not a `dict`.

### Explicit Over Magic

Retries, streaming, budgets, and memory policies are observable and configurable. No hidden behavior.

### Composition Over Framework Lock-In

Diamond architecture — `lellm-graph` and `lellm-provider` are peer layers, both built on `lellm-core`:

```
              lellm-core (protocol types)
             /                \
            /                  \
  lellm-provider         lellm-graph
  (LLM adapters)         (workflow engine)
            \                  /
             \                /
      lellm-agent (ReAct = internal graph)
```

- **lellm-core** — Zero-runtime protocol types. Standalone.
- **lellm-provider** — Provider adapters only. No graph, no agent.
- **lellm-graph** — Generic workflow engine. **Zero LLM dependency.**
- **lellm-agent** — Composes provider + graph. ReAct loop is an internal graph.

### Graph is Runtime, Agent is DSL

The ReAct agent loop is **not** a hand-rolled `while` loop — it's an internal graph:

```
START → budget_check ──(ok)──→ [llm] → [post_llm_check]
         │                         │              │
      (compact) → [compactor]     │       has_tools → [tool] → budget_check (loop)
                                   │       no_tools  → [end]
                              (thinking)
```

This means every graph feature — checkpointing, barriers, parallel execution, tracing — works for agents too.

---

## Feature Overview

### Provider Abstraction

One unified API for all LLM providers:

| Provider | Codec | Streaming | Tools |
|---|---|---|---|
| OpenAI | `OpenAICompatCodec::openai()` | ✅ | ✅ |
| Anthropic | `AnthropicCodec` | ✅ | ✅ |
| Google | `GoogleCodec` | ✅ | ✅ |
| DeepSeek | `OpenAICompatCodec::deepseek()` | ✅ | ✅ |
| NVIDIA | `OpenAICompatCodec::nvidia()` | ✅ | ✅ |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` | ✅ | ✅ |

Provider integration is separated into three concerns: `ChatCodec + ModelCapabilities + ProviderMeta`.

### Agent Runtime

- **ReAct loop** as internal graph — not a `while` loop
- **Tool use** with typed arguments, auto-generated JSON Schema
- **Context budget** — hard token limits, pluggable compaction
- **Retry policy** — configurable backoff, `ParallelSafety` categories
- **Streaming** — `AgentStream` with `AgentEvent` events
- **Fallback strategy** — graceful degradation on LLM errors

### Graph Orchestration

`lellm-graph` is a generic workflow engine — similar in scope to LangGraph / Temporal / Prefect.
**Zero LLM dependency.**

| Node | Purpose |
|---|---|
| `TaskNode` | Simple function node |
| `ConditionNode` | Conditional branching by state |
| `BarrierNode` | Human-in-the-loop — pause, decide, resume |
| `ParallelNode` | Fan-out / fan-in with merge strategy |

Three execution modes: **blocking**, **streaming** (channel-based), **inline** (zero-channel overhead).

### Checkpoint & Trace

- **Checkpoint** — persist state at node boundaries, resume from failure
- **Mutation Log** — every state change recorded as typed mutation
- **Execution Trace** — audit trail of every step, exportable to JSON
- **Barrier Re-Wait** — re-engage human approval on recovery

### Tool System

Two ways to define tools:

```rust
// Option 1: #[tool] macro — 95% of cases
#[tool(name = "get_weather", description = "Get current weather")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// Option 2: #[derive(Tool)] — full control over struct
#[derive(Deserialize, JsonSchema, Tool)]
struct GetWeatherArgs {
    /// City name
    city: String,
}
```

Tools are **never** `dict` — `ToolArgs` is a strongly-typed Rust struct with auto-generated JSON Schema.

### MCP Integration

Built-in MCP client/server support with dynamic tool catalogs, conflict resolution, and registry management.

---

## Installation

```toml
[dependencies]
# Default: core + provider adapters
lellm = "0.4"

# Agent runtime (includes core + graph + provider + agent)
lellm = { version = "0.4", features = ["agent"] }

# Everything
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
├── lellm-derive/        # Derive + attribute macros (proc-macro)
└── lellm-mcp/           # MCP (Model Context Protocol) client/server
    └── deps: lellm-core + optional lellm-agent
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
| **v0.1** | Provider abstraction, streaming, tool execution, budget enforcement | ✅ Done |
| **v0.2** | Graph orchestration, provider extension API, memory architecture | ✅ Done |
| **v0.3** | Agent graph runtime — ReAct loop, barriers, multi-agent coordination | ✅ Done |
| **v0.4** | ReAct as internal graph, typed state, checkpoint, trace, parallel execution | ✅ Done |
| **v0.5** | Graph is Runtime, Agent is DSL, checkpoint projection, execution session | 🔜 In Progress |
| **v0.6** | Distributed execution, visual observability, human-in-the-loop SDK | Planned |

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
