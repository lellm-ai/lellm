# LeLLM

English | [中文](./README_zh.md)

> LeLLM spreads joy. The most important thing in life is to be happy.

**Build stateful AI agents as typed directed graphs in Rust.**

Compile-time type safety. Durable checkpointing. Human-in-the-loop. No external services required.

[![crates.io](https://img.shields.io/crates/v/lellm.svg)](https://crates.io/crates/lellm)
[![License](https://img.shields.io/crates/l/lellm)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)

```rust
use lellm::prelude::*;
use std::sync::Arc;

let provider = CodecProvider::load(OpenAICompatCodec::openai())?;

#[tool(name = "get_weather", description = "Get current weather for a city")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

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

A complete **ReAct agent loop** — tool use, budget enforcement, retry policy, context compaction. Every type checked at compile time.

---

## Why LeLLM

Most AI frameworks optimize for speed of prototyping. **LeLLM optimizes for production reliability.**

| Challenge | LeLLM's Answer |
|---|---|
| Provider differences | Unified `ChatCodec` — one API, six providers |
| Runaway agent loops | Hard `max_iterations` + token budget |
| Context overflow | Pluggable compaction, observable token counts |
| Tool failures | Typed retry policy + `ParallelSafety` categories |
| State consistency | Checkpoint + Mutation Log + Execution Trace |
| Human approval gates | `BarrierNode` — pause, decide, resume |
| Parallel workflows | `ParallelNode` — fan-out / fan-in with typed merge |

---

## Core Concepts

### Graph is Runtime, Agent is DSL

Every agent is a compiled directed graph. The ReAct loop is not a `while` — it's a real graph:

```
START → budget_check ──(ok)──→ [llm] → [post_llm_check]
         │                         │              │
      (compact) → [compactor]     │       has_tools → [tool] → budget_check
                                   │       no_tools  → [end]
```

Every graph feature — checkpointing, barriers, parallel execution, tracing — works for agents automatically.

### Durable Execution

Persist state at node boundaries, resume from exact failure point with graph hash validation:

```rust
let checkpoint = session.checkpoint();
// ... crash, restart, deploy ...
let restored = ExecutionSession::restore(checkpoint, graph)?;
```

### Human-in-the-Loop

Pause at any node, inspect or modify state, decide to approve / reject / modify / reroute:

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

// Barrier pauses — your control plane decides:
handle.decide(barrier_id, BarrierDecision::Approve).await?;
```

### Type-Safe Everything

State is a struct, not a `dict`. Tool args are Rust types with auto-generated JSON Schema. Invalid states fail at compile time.

### Multi-Provider, One API

| Provider | Codec | Streaming | Tool Use |
|---|---|---|---|
| OpenAI | `OpenAICompatCodec::openai()` | ✅ | ✅ |
| Anthropic | `AnthropicCodec` | ✅ | ✅ |
| Google | `GoogleCodec` | ✅ | ✅ |
| DeepSeek | `OpenAICompatCodec::deepseek()` | ✅ | ✅ |
| NVIDIA | `OpenAICompatCodec::nvidia()` | ✅ | ✅ |
| vLLM / LLaMA | `OpenAICompatCodec::vllm()` | ✅ | ✅ |

---

## Installation

```bash
cargo add lellm
```

```toml
[dependencies]
# Default: core + provider adapters
lellm = "0.4"

# Agent runtime (core + graph + provider + agent)
lellm = { version = "0.4", features = ["agent"] }

# Everything
lellm = { version = "0.4", features = ["full"] }
```

| Feature | Includes |
|---|---|
| `provider` (default) | core + LLM adapters |
| `graph` | standalone workflow engine — zero LLM dependency |
| `agent` | full agent runtime — ReAct + tools + checkpoint |
| `mcp` | MCP client/server |
| `derive` | `#[tool]` and `#[derive(Tool)]` macros |
| `full` | everything |

**Requirements:** Rust 2024 edition, stable toolchain.

---

## Architecture

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

- **lellm-core** — Protocol types. Standalone.
- **lellm-provider** — Provider adapters. No graph, no agent.
- **lellm-graph** — Workflow engine. **Zero LLM dependency.**
- **lellm-agent** — Composes provider + graph.
- **lellm-mcp** — MCP client/server.
- **lellm-derive** — Proc-macros.

Each crate is independently usable.

---

## How LeLLM Compares

| | **LeLLM** | **LangGraph** |
|---|---|---|
| Language | Rust | Python |
| Type Safety | Compile-time | Runtime (TypedDict) |
| Agent = Graph | Yes — compiled internal graph | Yes — StateGraph |
| Graph Engine | Built-in, zero LLM dependency | Built-in |
| Checkpointing | Built-in, typed, mutation log | Built-in |
| Human-in-the-Loop | `BarrierNode` with routing | `interrupt()` |
| Streaming | Decoupled pipeline | Multiple modes |
| Runtime | No GIL, true parallelism | asyncio |
| Deploy | Anywhere Rust runs | Python runtime |
| Observability | Built-in Trace + Mutation Log | LangSmith (cloud) |

LeLLM's graph engine has **zero LLM dependency** and **built-in observability** — no paid cloud service needed.

---

## Roadmap

| Version | Scope | Status |
|---|---|---|
| **v0.1** | Provider abstraction, streaming, tool execution | ✅ Done |
| **v0.2** | Graph orchestration, provider extension API | ✅ Done |
| **v0.3** | Agent graph runtime — ReAct loop, barriers | ✅ Done |
| **v0.4** | ReAct as internal graph, typed state, checkpoint, trace | ✅ Done |
| **v0.5** | Graph is Runtime, Agent is DSL, execution session | 🔜 In Progress |
| **v0.6** | Distributed execution, visual observability | Planned |

---

## Learn More

- [Blueprint](./docs/BLUEPRINT.md) — Product blueprint and API contracts
- [Design Doc](./docs/DESIGN.md) — Key design decisions
- [LangGraph vs LeLLM Graph](./docs/langgraph-vs-lellm-graph.md) — Architecture comparison

## License

MIT
