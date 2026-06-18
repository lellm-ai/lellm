# LeLLM v3 架构重构实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use compose:subagent (recommended) or compose:execute to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 9 crate 架构重构为 6 crate 精简架构，消除概念污染，建立清晰的执行层/遥测层边界。

**Architecture:** core（纯协议）→ graph（吸收 runtime）→ provider（三权分立）→ agent（智能体）→ mcp（独立协议域）→ derive（宏）。events crate 消除，TraceId/SpanId 迁移到 graph。

**Tech Stack:** Rust 2024 edition, async-trait, tokio, serde, thiserror, tracing

---

## 最终依赖图

```
         lellm
           │
     ┌─────┼─────┬─────┐
     ▼     ▼     ▼     ▼
   graph  agent  mcp  derive
     │     │     │
     ▼     ▼     ▼
   core  provider core
```

## 红线

1. `graph ↛ agent`
2. `provider ↛ graph`
3. `mcp ↛ agent`

---

## Phase 1: lellm-core 清理 — 移除 TraceId/SpanId

### 目标
core 不再包含 trace 概念，只保留纯协议类型。

### Task 1.1: 从 core 移除 TraceId/SpanId

**Files:**
- Modify: `lellm-core/src/ids.rs` — 删除整个文件
- Modify: `lellm-core/src/lib.rs:19` — 移除 `pub use ids::{SpanId, TraceId};`
- Modify: `lellm-core/Cargo.toml` — 移除 `uuid` 依赖

- [ ] **Step 1: 从 lib.rs 移除 ids 模块引用**

```rust
// lellm-core/src/lib.rs
// 删除第 10 行: pub mod ids;
// 删除第 19 行: pub use ids::{SpanId, TraceId};
```

- [ ] **Step 2: 删除 ids.rs 文件**

```bash
rm lellm-core/src/ids.rs
```

- [ ] **Step 3: 从 Cargo.toml 移除 uuid 依赖**

```toml
# lellm-core/Cargo.toml — 删除 uuid 行
# uuid.workspace = true   ← 删除
```

- [ ] **Step 4: 验证 core 编译**

Run: `cargo check -p lellm-core`
Expected: 编译通过

- [ ] **Step 5: 运行 core 测试**

Run: `cargo test -p lellm-core`
Expected: 全部通过

- [ ] **Step 6: Commit**

```bash
git add lellm-core/
git commit -m "refactor(core): remove TraceId/SpanId — pure protocol layer"
```

---

## Phase 2: lellm-graph 吸收 lellm-runtime + TraceId/SpanId + Events

### 目标
graph 成为完整的执行引擎，包含 State/StateDelta/Checkpoint/TraceId/SpanId + GraphEvent/FlowEvent。

### Task 2.1: 将 lellm-runtime 代码合并到 lellm-graph

**Files:**
- Create: `lellm-graph/src/state/mod.rs` — 从 lellm-runtime/src/state.rs 迁移
- Create: `lellm-graph/src/delta.rs` — 从 lellm-runtime/src/delta.rs 迁移
- Create: `lellm-graph/src/statekey.rs` — 从 lellm-runtime/src/statekey.rs 迁移
- Create: `lellm-graph/src/checkpoint.rs` — 从 lellm-runtime/src/checkpoint.rs 迁移（当前已有同名文件，需要合并）
- Create: `lellm-graph/src/store.rs` — 从 lellm-runtime/src/store.rs 迁移
- Create: `lellm-graph/src/ids.rs` — 从 lellm-core/src/ids.rs 迁移（TraceId/SpanId）
- Modify: `lellm-graph/Cargo.toml` — 移除 `lellm-runtime` 和 `lellm-events` 依赖，新增 `uuid`
- Modify: `lellm-graph/src/lib.rs` — 更新 re-export

- [ ] **Step 1: 将 TraceId/SpanId 迁移到 graph**

```rust
// lellm-graph/src/ids.rs — 从 lellm-core/src/ids.rs 迁移
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TraceId(pub Uuid);

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SpanId(pub Uuid);

impl Default for SpanId {
    fn default() -> Self {
        Self::new()
    }
}

impl SpanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for SpanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
```

- [ ] **Step 2: 将 State/StateDelta/Reducer 迁移到 graph**

```bash
# 从 lellm-runtime/src/ 复制文件到 lellm-graph/src/
cp lellm-runtime/src/state.rs lellm-graph/src/state_original.rs
cp lellm-runtime/src/delta.rs lellm-graph/src/delta.rs
cp lellm-runtime/src/statekey.rs lellm-graph/src/statekey.rs
cp lellm-runtime/src/store.rs lellm-graph/src/store.rs
```

需要将 `use lellm_core::SpanId` 改为 `use crate::ids::SpanId`。

- [ ] **Step 3: 将 Checkpoint 迁移到 graph（合并到现有 checkpoint.rs）**

当前 `lellm-graph/src/checkpoint.rs` 已有 `GraphHashMode`、`ExecutionTrace` 等。需要：
- 从 `lellm-runtime/src/checkpoint.rs` 迁移 `Checkpoint`、`CheckpointStore`、`CheckpointPolicy`、`CheckpointTrigger`、`CheckpointScore`、`CheckpointId`、`ExecutionEntry`、`ExecutionMetadata`、`IncrementalSnapshotState`、`GraphResult`
- 将 `pub use lellm_core::TraceId` 改为 `use crate::ids::TraceId`
- 合并到现有 checkpoint.rs 中

- [ ] **Step 4: 将 GraphEvent/FlowEvent 迁移到 graph**

```bash
# 从 lellm-events/src/lib.rs 中提取 GraphEvent/FlowEvent 相关代码
# 迁移到 lellm-graph/src/event.rs（当前已存在，需要扩展）
```

需要：
- 将 `AgentEvent` 保留在 `lellm-agent` 中
- 将 `GraphEvent`、`FlowEvent`、`GraphCompleteResult`、`StateSnapshot`、`ObservedError`、`BarrierId`、`BarrierDecision` 迁移到 `lellm-graph/src/event.rs`
- 将 `agent_event_to_flow_event` 和 `extract_agent_event` 移到 `lellm-agent` 中
- 将 `use lellm_core::{SpanId, TraceId}` 改为 `use crate::ids::{SpanId, TraceId}`
- 将 `use lellm_runtime::StateDelta` 改为 `use crate::delta::StateDelta`

- [ ] **Step 5: 更新 lellm-graph Cargo.toml**

```toml
[dependencies]
lellm-core.workspace = true
# 移除: lellm-events.workspace = true
# 移除: lellm-runtime.workspace = true
uuid.workspace = true  # 新增
async-trait.workspace = true
indexmap.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
tracing.workspace = true
thiserror.workspace = true
```

- [ ] **Step 6: 更新 lellm-graph/src/lib.rs**

移除 `pub use lellm_runtime::*` 和 `pub use lellm_events::*`，改为直接 export 本 crate 内的类型。保留原有的 `graph`、`node`、`executor`、`error`、`hook`、`barrier_node`、`parallel_node` 模块，新增 `ids`、`delta`、`state`、`statekey`、`store`、`event` 模块。

- [ ] **Step 7: 修复所有内部引用**

将所有 `lellm_runtime::*` 改为 `crate::*`，将所有 `lellm_core::{TraceId, SpanId}` 改为 `crate::ids::{TraceId, SpanId}`。

- [ ] **Step 8: 验证 graph 编译**

Run: `cargo check -p lellm-graph`
Expected: 编译通过

- [ ] **Step 9: 运行 graph 测试**

Run: `cargo test -p lellm-graph`
Expected: 全部通过

- [ ] **Step 10: Commit**

```bash
git add lellm-graph/
git commit -m "refactor(graph): absorb lellm-runtime + events — unified execution engine"
```

### Task 2.2: 删除 lellm-runtime crate

- [ ] **Step 1: 从 workspace 移除**

```toml
# Cargo.toml (workspace root)
# members 中移除 "lellm-runtime"
# [workspace.dependencies] 中移除 lellm-runtime
```

- [ ] **Step 2: 删除 crate 目录**

```bash
# 安全检查：确认没有其他 crate 依赖 lellm-runtime
rg "lellm-runtime" --include "*.toml"
# 除了 lellm-graph 以外不应有其他引用（已清理）
```

- [ ] **Step 3: 验证全量编译**

Run: `cargo check --workspace`
Expected: 编译通过

- [ ] **Step 4: Commit**

```bash
git rm -r lellm-runtime/
git commit -m "chore: remove lellm-runtime — absorbed into lellm-graph"
```

### Task 2.3: 删除 lellm-events crate

- [ ] **Step 1: 从 workspace 移除**

```toml
# Cargo.toml (workspace root)
# members 中移除 "lellm-events"
# [workspace.dependencies] 中移除 lellm-events
```

- [ ] **Step 2: 删除 crate 目录**

```bash
# 安全检查
rg "lellm-events" --include "*.toml"
# 确认无其他引用
```

- [ ] **Step 3: 验证全量编译**

Run: `cargo check --workspace`
Expected: 编译通过

- [ ] **Step 4: Commit**

```bash
git rm -r lellm-events/
git commit -m "chore: remove lellm-events — events return to their domains"
```

---

## Phase 3: lellm-provider 清理

### 目标
provider 只依赖 core，保持三权分立，移除不必要的依赖。

### Task 3.1: 确认 provider 依赖纯净

当前 `lellm-provider/Cargo.toml` 依赖 `lellm-core`，这是正确的。检查是否需要移除 `anyhow` 依赖。

- [ ] **Step 1: 检查 anyhow 使用情况**

Run: `rg "anyhow" lellm-provider/src/`
Expected: 如果无使用则移除

- [ ] **Step 2: 如果有使用，评估是否可以替换为 thiserror**

- [ ] **Step 3: 验证编译**

Run: `cargo check -p lellm-provider`
Expected: 编译通过

- [ ] **Step 4: 运行测试**

Run: `cargo test -p lellm-provider`
Expected: 全部通过

- [ ] **Step 5: Commit（如果有变更）**

```bash
git add lellm-provider/
git commit -m "refactor(provider): clean up dependencies"
```

---

## Phase 4: lellm-mcp 清理

### 目标
mcp 依赖 core + graph，移除对 agent 的可选依赖。

### Task 4.1: 更新 mcp 依赖

**Files:**
- Modify: `lellm-mcp/Cargo.toml` — 移除 `lellm-agent` 依赖，新增 `lellm-graph`
- Modify: `lellm-mcp/src/lib.rs` — 更新 cfg feature gate

- [ ] **Step 1: 更新 Cargo.toml**

```toml
[dependencies]
lellm-core.workspace = true
lellm-graph.workspace = true  # 新增（替代可选的 lellm-agent）
# 移除: lellm-agent = { workspace = true, optional = true }

[features]
default = ["stdio", "bridge"]
stdio = []
bridge = ["lellm-graph"]  # 从 "lellm-agent" 改为 "lellm-graph"
```

- [ ] **Step 2: 更新 lib.rs 中的 feature gate**

将 `#[cfg(feature = "bridge")]` 保持不变（bridge 功能现在依赖 graph 而非 agent）。

- [ ] **Step 3: 检查 bridge 模块是否有 agent 特有引用**

Run: `rg "lellm_agent" lellm-mcp/src/`
Expected: 如果有引用需要修改

- [ ] **Step 4: 验证编译**

Run: `cargo check -p lellm-mcp`
Expected: 编译通过

- [ ] **Step 5: 运行测试**

Run: `cargo test -p lellm-mcp`
Expected: 全部通过

- [ ] **Step 6: Commit**

```bash
git add lellm-mcp/
git commit -m "refactor(mcp): depend on graph instead of agent"
```

---

## Phase 5: lellm-agent 清理

### 目标
agent 依赖 core + graph + provider，移除 events 依赖，AgentEvent 留在 agent 中。

### Task 5.1: 更新 agent 依赖

**Files:**
- Modify: `lellm-agent/Cargo.toml` — 移除 `lellm-events` 和 `lellm-runtime` 依赖
- Modify: `lellm-agent/src/lib.rs` — 更新 re-export

- [ ] **Step 1: 更新 Cargo.toml**

```toml
[dependencies]
lellm-core.workspace = true
lellm-graph.workspace = true   # 已有
lellm-provider.workspace = true # 已有
# 移除: lellm-events.workspace = true
# 移除: lellm-runtime.workspace = true
```

- [ ] **Step 2: 将 AgentEvent 定义移到 agent crate 中**

从 `lellm-events/src/lib.rs` 中提取 `AgentEvent`、`LoopEndResult`、`StopReason`，放到 `lellm-agent/src/event.rs`。

```rust
// lellm-agent/src/event.rs
use lellm_core::ToolError;
use lellm_provider::ProviderEvent;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Provider(ProviderEvent),
    ToolStart { tool_call_id: String, name: String },
    ToolEnd {
        tool_call_id: String,
        result: Result<serde_json::Value, ToolError>,
    },
    Retry {
        tool_call_id: String,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    ContextCompacted {
        before_tokens: usize,
        after_tokens: usize,
        removed_messages: usize,
    },
    LoopEnd { result: LoopEndResult },
    LoopError {
        error: lellm_core::LlmError,
        iterations: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Complete,
    MaxIterationsReached,
    Cancelled,
    OutputBudgetExceeded,
    ReasoningBudgetExceeded,
}

#[derive(Debug, Clone)]
pub struct LoopEndResult {
    pub stop_reason: StopReason,
    pub iterations: usize,
    pub tool_calls_executed: usize,
}
```

- [ ] **Step 3: 将 agent_event_to_flow_event 适配器移到 agent 中**

```rust
// lellm-agent/src/event.rs — 追加
use lellm_graph::FlowEvent;

pub fn agent_event_to_flow_event(node_id: &str, event: AgentEvent) -> FlowEvent {
    FlowEvent::Custom {
        node_id: node_id.to_string(),
        payload: Box::new(event),
    }
}

pub fn extract_agent_event(event: &FlowEvent) -> Option<&AgentEvent> {
    match event {
        FlowEvent::Custom { payload, .. } => payload.downcast_ref::<AgentEvent>(),
        _ => None,
    }
}
```

- [ ] **Step 4: 更新 agent/src/lib.rs**

```rust
pub mod event;
pub mod hook;
pub mod runtime;

pub use event::{AgentEvent, LoopEndResult, StopReason, agent_event_to_flow_event, extract_agent_event};
```

- [ ] **Step 5: 更新 agent runtime 中的事件引用**

将 `lellm_events::AgentEvent` 替换为 `crate::event::AgentEvent`，将 `lellm_events::StopReason` 替换为 `crate::event::StopReason`。

Run: `rg "lellm_events" lellm-agent/src/`
Expected: 无引用

- [ ] **Step 6: 验证编译**

Run: `cargo check -p lellm-agent`
Expected: 编译通过

- [ ] **Step 7: 运行测试**

Run: `cargo test -p lellm-agent`
Expected: 全部通过

- [ ] **Step 8: Commit**

```bash
git add lellm-agent/
git commit -m "refactor(agent): own AgentEvent, remove events dependency"
```

---

## Phase 6: lellm-macros → lellm-derive 更名

### 目标
lellm-macros 更名为 lellm-derive。

### Task 6.1: 重命名 crate

**Files:**
- Rename: `lellm-macros/` → `lellm-derive/`
- Modify: `Cargo.toml` (workspace) — 更新 members 和 dependencies
- Modify: `lellm-derive/Cargo.toml` — 更新 package name
- Modify: `lellm-derive/src/lib.rs` — 更新 crate 级注释

- [ ] **Step 1: 重命名目录**

```bash
git mv lellm-macros lellm-derive
```

- [ ] **Step 2: 更新 lellm-derive/Cargo.toml**

```toml
[package]
name = "lellm-derive"
# ... 其余不变
```

- [ ] **Step 3: 更新 workspace Cargo.toml**

```toml
[workspace]
members = [
    # ...
    # "lellm-macros",  ← 移除
    "lellm-derive",     # 新增
    # ...
]

[workspace.dependencies]
# lellm-macros = { path = "lellm-macros", version = "0.2" }  ← 移除
lellm-derive = { path = "lellm-derive", version = "0.2" }    # 新增
```

- [ ] **Step 4: 更新所有引用 lellm-macros 的 Cargo.toml**

```bash
rg "lellm-macros" --include "*.toml"
```

需要更新：
- `lellm-agent/Cargo.toml` (dev-dependencies)
- `lellm-graph/Cargo.toml` (dev-dependencies)
- `lellm/Cargo.toml` (dependencies + features)

- [ ] **Step 5: 更新所有 `use lellm_macros` 的代码**

```bash
rg "lellm_macros" --include "*.rs"
```

- [ ] **Step 6: 验证编译**

Run: `cargo check --workspace`
Expected: 编译通过

- [ ] **Step 7: Commit**

```bash
git add .
git commit -m "refactor: rename lellm-macros to lellm-derive"
```

---

## Phase 7: lellm facade 重构

### 目标
feature gate 设计：`default = ["provider"]`。

### Task 7.1: 更新 facade Cargo.toml

**Files:**
- Modify: `lellm/Cargo.toml` — 更新 features
- Modify: `lellm/src/lib.rs` — 更新 re-export

- [ ] **Step 1: 更新 Cargo.toml**

```toml
[features]
default = ["provider"]
graph = ["dep:lellm-core", "dep:lellm-graph"]
provider = ["dep:lellm-core", "dep:lellm-provider"]
agent = ["dep:lellm-core", "dep:lellm-graph", "dep:lellm-provider", "dep:lellm-agent"]
mcp = ["dep:lellm-core", "dep:lellm-graph", "dep:lellm-mcp"]
derive = ["dep:lellm-derive"]
full = ["graph", "provider", "agent", "mcp", "derive"]

[dependencies]
lellm-core = { workspace = true, optional = true }
lellm-provider = { workspace = true, optional = true }
lellm-graph = { workspace = true, optional = true }
lellm-agent = { workspace = true, optional = true }
lellm-mcp = { workspace = true, optional = true }
lellm-derive = { workspace = true, optional = true }
```

- [ ] **Step 2: 更新 lib.rs**

```rust
#[cfg(feature = "core")]
pub use lellm_core as core;

#[cfg(feature = "provider")]
pub use lellm_provider as provider;

#[cfg(feature = "graph")]
pub use lellm_graph as graph;

#[cfg(feature = "agent")]
pub use lellm_agent as agent;

#[cfg(feature = "mcp")]
pub use lellm_mcp as mcp;

#[cfg(feature = "derive")]
pub use lellm_derive as derive;
```

- [ ] **Step 3: 验证编译**

Run: `cargo check -p lellm`
Expected: 编译通过

Run: `cargo check -p lellm --features full`
Expected: 编译通过

- [ ] **Step 4: 运行全量测试**

Run: `cargo test --workspace`
Expected: 全部通过

- [ ] **Step 5: Commit**

```bash
git add lellm/
git commit -m "refactor(facade): new feature gate design — default = provider"
```

---

## Phase 8: 全量验证

### Task 8.1: 全量编译和测试

- [ ] **Step 1: 清理编译缓存**

Run: `cargo clean`

- [ ] **Step 2: 全量编译**

Run: `cargo check --workspace`
Expected: 编译通过

- [ ] **Step 3: 全量测试**

Run: `cargo test --workspace`
Expected: 全部通过

- [ ] **Step 4: 检查 examples 编译**

Run: `cargo check --workspace --examples`
Expected: 编译通过

- [ ] **Step 5: 格式化**

Run: `cargo fmt --all`
Expected: 无格式变更

- [ ] **Step 6: Clippy**

Run: `cargo clippy --workspace -- -D warnings`
Expected: 无警告

- [ ] **Step 7: 最终 Commit**

```bash
git add .
git commit -m "chore: v3 architecture refactor complete — 6 crate architecture"
```

---

## 文件清单总结

### 删除的 crate
- `lellm-runtime/` — 吸收到 graph
- `lellm-events/` — 事件回归各自领域

### 重命名的 crate
- `lellm-macros/` → `lellm-derive/`

### 新增的文件
- `lellm-graph/src/ids.rs` — TraceId/SpanId
- `lellm-graph/src/delta.rs` — StateDelta, Reducer
- `lellm-graph/src/state/` — State, StateExt
- `lellm-graph/src/statekey.rs` — StateKey
- `lellm-graph/src/store.rs` — InMemoryCheckpointStore
- `lellm-agent/src/event.rs` — AgentEvent, StopReason

### 修改的关键文件
- `lellm-core/src/lib.rs` — 移除 ids 模块
- `lellm-graph/Cargo.toml` — 移除 runtime/events 依赖
- `lellm-graph/src/lib.rs` — 吸收 runtime 模块
- `lellm-graph/src/checkpoint.rs` — 合并 runtime checkpoint
- `lellm-graph/src/event.rs` — 合并 events crate 的 GraphEvent/FlowEvent
- `lellm-agent/Cargo.toml` — 移除 events 依赖
- `lellm-agent/src/lib.rs` — 新增 event 模块
- `lellm-mcp/Cargo.toml` — 依赖改为 core + graph
- `lellm/Cargo.toml` — feature gate 重构
