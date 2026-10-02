# LeLLM

English | [中文](./README_zh.md)

> LeLLM spreads joy. The most important thing in life is to be happy.

**Build AI agents with an inspectable mind.**

Every agent is a compiled directed graph — not a black-box `while` loop. Compile-time type safety. State checkpointing at node boundaries. Human-in-the-loop barriers. No external services required.

[![crates.io](https://img.shields.io/crates/v/lellm.svg)](https://crates.io/crates/lellm)
[![License](https://img.shields.io/crates/l/lellm)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-2024-orange)](https://www.rust-lang.org)

```rust
use lellm::prelude::*;

let provider = CodecProvider::load(OpenAICompatCodec::openai())?;
let model = ResolvedModel::new(provider, "gpt-6.1-sol");

#[tool(name = "get_weather", description = "Get current weather for a city")]
async fn get_weather(city: String) -> ToolResult {
    Ok(serde_json::json!({"city": city, "temp": 25}))
}

let agent = AgentBuilder::new(model)
    .system("You are a helpful assistant.")
    .tool(get_weather_tool()) // #[tool] macro generates {fn}_tool()
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

Most agent frameworks hide the loop. LeLLM compiles it into a graph you can see, pause, and resume.

| What You Fight With Black-Box Loops | How LeLLM Solves It |
|---|---|
| Unbounded agent loops spinning forever | Hard `max_iterations` + token budget, enforced at graph boundaries |
| Context window overflow mid-conversation | Pluggable compaction node, observable token counts |
| Tool failures crash the entire run | Typed retry policy + `ParallelSafety` categories |
| Restart loses in-flight state | Node-boundary state checkpoint + Execution Trace (durable resume + Mutation Log on roadmap) |
| "Trust me it works" runtime types | Rust structs — invalid states fail at compile time |
| Observability requires a paid cloud | Built-in Execution Trace — zero SaaS dependency (Mutation Log on roadmap) |

---

## Core Concepts

### Every Agent is a Compiled Graph

```
START → budget_check ──(ok)──→ [llm] → [post_llm_check]
         │                         │              │
      (compact) → [compactor]     │       has_tools → [tool] → budget_check
                                   │       no_tools  → [end]
```

The ReAct loop is not a `while` — it's a real directed graph with typed nodes and edges. Graph features — barriers, parallel execution, tracing — work for agents; the agent's **non-streaming** entry points now support checkpoint save/restore (explicit calls, not automatic).

### State Checkpointing (Durable Execution)

Snapshot state **and the execution cursor** at node boundaries; a new process resumes from the checkpoint without rerunning committed nodes:

```rust
let store = Arc::new(FileBlobStore::new("./checkpoints"));
let config = CheckpointConfig::for_store(
    store,
    SerdeCheckpointCodec::new(),
    graph.canonical_hash(),
);
let exec = executor.execute_stream_with_checkpoint(graph.clone(), state, config.clone())?;
// ... process crash (panic / SIGKILL) ... (new process)
let cp = typed.load_latest(&trace_id, graph.canonical_hash()).await?;
let exec = executor
    .execute_stream_with_restore(graph, cp, trace_id, config)
    .await?;
```

> **Scope**: serial graphs (including loops). Graphs containing Parallel / Subgraph / Barrier are rejected **at the persistence entry**, not after a crash. Checkpoint/restore is wired into the graph execution path and the agent's **non-streaming** entry points (`invoke_with_checkpoint` / `invoke_with_restore`); the streaming path and auto-checkpointing on `invoke()` remain on the roadmap.
>
> **Durability boundary**: flush + rename guarantees write completion and atomic visibility — **process-crash safe** (panic / SIGKILL). It does **not** guarantee visibility after power loss (requires file + directory fsync; phase 2).
>
> **No exactly-once**: if the process dies after a node's side effect succeeded but before the checkpoint landed, restore reruns that node. Use idempotency keys / dedup / business compensation.

### Human-in-the-Loop

Pause at a node and wait for a decision:

> **Current behavior**: `Approve` (continue) and `Reroute` (jump to a node) are fully supported; `Cancelled` aborts. `Reject`/`Modify`/timeout decision application is being wired (see roadmap).

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

handle.decide(barrier_id, BarrierDecision::Approve).await?;
```

---

## Installation

```bash
cargo add lellm
```

```toml
[dependencies]
lellm = "0.4"                                    # core + provider adapters
lellm = { version = "0.4", features = ["agent"] } # full agent runtime
lellm = { version = "0.4", features = ["full"] }  # everything
# lightweight MCP: stdio transport only, no agent/provider (no reqwest/hyper/TLS)
lellm = { version = "0.4", default-features = false, features = ["mcp-stdio"] }
```

| Feature | Includes |
|---|---|
| `provider` (default) | core + LLM adapters |
| `graph` | standalone workflow engine — **zero LLM dependency** |
| `agent` | full agent runtime — ReAct + tools (checkpoint save/restore on non-streaming entry; auto-checkpoint on roadmap) |
| `mcp` | MCP client/server + agent integration (stdio/http/sse) |
| `mcp-stdio` | lightweight MCP client/server — stdio only, **no agent/provider/reqwest** |
| `derive` | `#[tool]` and `#[derive(Tool)]` macros |

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
| Type Safety | Compile-time (Rust structs) | Runtime (TypedDict) |
| Agent = Graph | Yes — compiled internal graph | Yes — StateGraph |
| Graph Engine | Built-in, **zero LLM dependency** | Built-in |
| Checkpointing | Built-in, typed state snapshot (durable resume on roadmap) | Built-in |
| Human-in-the-Loop | `BarrierNode` with routing | `interrupt()` |
| Streaming | Decoupled pipeline | Multiple modes |
| Runtime | No GIL, true parallelism | asyncio (GIL-bounded) |
| Observability | **Built-in** Execution Trace (Mutation Log on roadmap) | LangSmith (cloud service) |
| Deploy | Anywhere Rust runs | Python runtime |

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
