# LeLLM

English | [中文](./README_zh.md)

> LeLLM spreads joy. The most important thing in life is to be happy.

**Graph-native agent orchestration in Rust.**

Build stateful, long-running AI agents as typed directed graphs — with compile-time safety, durable checkpointing, human-in-the-loop, and zero external services required.

[![crates.io](https://img.shields.io/crates/v/lellm.svg)](https://crates.io/crates/lellm)
[![License](https://img.shields.io/crates/l/lellm)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)

```bash
cargo add lellm
```

---

## Quick Start

```rust
use lellm::prelude::*;
use std::sync::Arc;

// 1. Provider — auto-loads from OPENAI_API_KEY
let provider = CodecProvider::load(OpenAICompatCodec::openai())?;

// 2. Define a tool — #[tool] auto-generates JSON Schema from the fn signature
#[tool(name = "get_weather", description = "Get current weather for a city")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

// 3. Build & run an agent
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

let result = agent
    .invoke(vec![Message::user_text("What's the weather in Shanghai?")])
    .await?;

match result.stop_reason {
    StopReason::Complete => println!("Done in {} iterations", result.iterations),
    StopReason::MaxIterationsReached => eprintln!("Max iterations reached"),
    _ => eprintln!("Stopped: {:?}", result.stop_reason),
}
```

That's a complete **ReAct agent loop** — with tool use, budget enforcement, retry policy, and context compaction. No hidden state. No runtime magic. Every type is checked at compile time.

---

## Why LeLLM

Most AI frameworks optimize for speed of prototyping. **LeLLM optimizes for production reliability.**

The hard parts of building AI systems aren't calling an API — they're keeping the system correct under load:

| Challenge | LeLLM's Answer |
|---|---|
| Provider differences | Unified `ChatCodec` — one API, six providers |
| Runaway agent loops | Hard `max_iterations` + token budget, enforced at runtime |
| Context overflow | Pluggable compaction strategy, observable token counts |
| Tool execution failures | Typed retry policy + `ParallelSafety` concurrency categories |
| Partial stream failures | Decoupled streaming pipeline — pure logic, zero IO coupling |
| State consistency | Checkpoint + Mutation Log + Execution Trace audit trail |
| Human approval gates | `BarrierNode` — pause, decide, resume from exact state |
| Parallel workflows | `ParallelNode` — fan-out / fan-in with typed merge strategy |

---

## Core Benefits

### Graph is Runtime, Agent is DSL

Every agent in LeLLM is a compiled directed graph. The ReAct loop isn't a hand-rolled `while` — it's:

```
START → budget_check ──(ok)──→ [llm] → [post_llm_check]
         │                         │              │
      (compact) → [compactor]     │       has_tools → [tool] → budget_check
                                   │       no_tools  → [end]
```

This means **every graph feature works for agents**: checkpointing, barriers, parallel execution, tracing. You get durable execution without writing extra code.

### Durable Execution

Build agents that persist through failures and resume from exactly where they left off:

```rust
use lellm::prelude::*;

// Create session with graph
let session = ExecutionSession::new(state, graph.clone());

// Save checkpoint — serializable to any backend (file, S3, Redis)
let checkpoint = session.checkpoint();

// ... crash, restart, deploy ...

// Restore — graph hash is auto-validated, mismatch rejects recovery
let restored = ExecutionSession::restore(checkpoint, graph)?;
```

- **Checkpoint** — persist typed state at every node boundary
- **Mutation Log** — every state change recorded as a typed mutation
- **Execution Trace** — full audit trail, exportable to JSON
- **Barrier Re-Wait** — re-engage human approval on recovery

### Human-in-the-Loop

Pause agent execution at any point, inspect or modify state, and resume:

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

// The BarrierNode pauses execution and emits a BarrierId.
// Your control plane decides: approve, reject, modify, or reroute.
handle.decide(barrier_id, BarrierDecision::Approve).await?;
```

### Type-Safe State

State is a strongly-typed struct, not a `dict`. Tool arguments are Rust structs with auto-generated JSON Schema. Invalid states fail at compile time.

```rust
// Tool args are strongly-typed — no "what keys does this tool accept?" guessing
#[derive(Deserialize, JsonSchema, Tool)]
struct SearchArgs {
    /// The query to search for
    query: String,
    /// Maximum number of results
    #[serde(default = "default_limit")]
    limit: u32,
}
```

### Multi-Provider, One API

| Provider | Codec | Streaming | Tool Use |
|---|---|---|---|
| OpenAI | `OpenAICompatCodec::openai()` | ✅ | ✅ |
| Anthropic | `AnthropicCodec` | ✅ | ✅ |
| Google | `GoogleCodec` | ✅ | ✅ |
| DeepSeek | `OpenAICompatCodec::deepseek()` | ✅ | ✅ |
| NVIDIA | `OpenAICompatCodec::nvidia()` | ✅ | ✅ |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` | ✅ | ✅ |

Swap providers by changing one line. The `ChatCodec` abstraction handles protocol encoding, streaming parsing, and capability negotiation.

---

## Common Patterns

### Streaming Agent Execution

```rust
use futures_util::StreamExt;

let mut stream = agent.invoke_stream(vec![Message::user_text("Analyze this...")]);

while let Some(event) = stream.next().await {
    match event {
        AgentEvent::Provider(ProviderEvent::Token { token }) => print!("{}", token),
        AgentEvent::ToolStart { name, .. } => eprintln!("\n🔧 Calling: {}", name),
        AgentEvent::ToolEnd { result, .. } => eprintln!("✅ Result: {:?}", result),
        AgentEvent::LoopEnd { result } => {
            eprintln!("\nDone in {} iterations", result.iterations);
        }
        _ => {}
    }
}
```

### Custom Workflow Graph

`lellm-graph` has **zero LLM dependency** — use it as a general-purpose workflow engine:

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
    // Conditional: skip review for trusted sources
    .edge_if("review", "publish", |state: &State| is_trusted(state))
    .edge_fallback("review", "fetch")  // loop back on rejection
    .end("publish")
    .build()?;
```

### Parallel Branch Execution

```rust
let parallel = ParallelNode::builder()
    .branch("translate", TaskNode::new(translate_fn))
    .branch("summarize", TaskNode::new(summarize_fn))
    .branch("extract", TaskNode::new(extract_fn))
    .error_strategy(ParallelErrorStrategy::CollectAll)
    .build();
```

---

## How LeLLM Compares

| | **LeLLM** | **LangGraph (Python)** | **AutoGen** |
|---|---|---|---|
| **Language** | Rust | Python | Python/TS |
| **Type Safety** | Compile-time (struct state, typed mutations) | Runtime (TypedDict) | Minimal |
| **Agent = Graph** | Yes — ReAct is a compiled internal graph | Yes — StateGraph | No — linear chains |
| **Graph Engine** | Built-in, zero LLM dependency | Built-in | External |
| **Checkpointing** | Built-in, typed, with mutation log | Built-in (checkpointer) | Limited |
| **Human-in-the-Loop** | `BarrierNode` with decision routing | `interrupt()` | Limited |
| **Streaming** | Decoupled pipeline (pure logic, no IO) | Multiple stream modes | Basic |
| **Runtime** | No GIL, true parallelism | asyncio (single-threaded event loop) | asyncio |
| **Deploy** | Anywhere Rust runs (server, edge, WASM) | Python runtime required | Python/Node required |
| **Observability** | Built-in Trace + Mutation Log | LangSmith (paid service) | LangChain tracing |
| **Philosophy** | Explicit, observable, type-safe | Convention over config | Conversational patterns |

**Key distinction:** LeLLM's graph engine has **zero LLM dependency** — `lellm-graph` depends only on `lellm-core` for protocol types. Observability is built-in (Trace + Mutation Log), not a paid cloud service.

---

## Who LeLLM Is For

### Built for you if you are:

- **Backend / infrastructure engineers** building AI-powered services
- **Platform teams** building agent runtimes or orchestration layers
- Building **performance-sensitive** applications (edge, embedded, low-latency)
- Teams requiring **deterministic runtime behavior** and built-in observability
- **Rust shops** wanting compile-time guarantees for AI systems

**Typical workloads:** AI APIs & gateways, internal copilots, agent runtimes, multi-agent orchestration, real-time streaming apps, long-running autonomous workflows.

### Probably not for you if:

- You primarily experiment in Jupyter notebooks
- Your app is just `HTTP → LLM → return` — `reqwest` + `serde` is enough
- You want no-code / low-code workflows
- You're learning Rust through AI projects

LeLLM starts paying off when **orchestration complexity** appears.

---

## Installation

```bash
cargo add lellm
```

Or pick the features you need:

```toml
[dependencies]
# Default: core + provider adapters (call LLMs directly)
lellm = "0.4"

# Agent runtime (core + graph + provider + agent)
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

**Requirements:** Rust 2024 edition, stable toolchain.

---

## Architecture

### Diamond Architecture

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

- **lellm-core** — Zero-runtime protocol types (`Message`, `ChatRequest`, `LlmError`). Standalone.
- **lellm-provider** — Provider adapters only. No graph, no agent. Standalone with core.
- **lellm-graph** — Generic workflow engine. **Zero LLM dependency.** Standalone with core.
- **lellm-agent** — Composes provider + graph. ReAct loop is an internal graph.
- **lellm-mcp** — MCP client/server. Independent protocol domain.
- **lellm-derive** — Proc-macro crate (`#[tool]`, `#[derive(Tool)]`).

### Crate Layout

```
lellm/
├── lellm/               # Facade — unified entry point
├── lellm-core/          # Protocol types (serde, thiserror)
├── lellm-provider/      # Provider adapters (core + reqwest + tokio)
├── lellm-graph/         # Workflow engine (core + tokio, NO LLM)
├── lellm-agent/         # Agent runtime (core + provider + graph)
├── lellm-derive/        # Derive + attribute macros (proc-macro)
└── lellm-mcp/           # MCP client/server (core + optional agent)
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
