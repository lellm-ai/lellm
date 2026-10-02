# Agent Checkpoint Phase 2 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 Agent 主路径（ToolUseLoop / ReAct graph）获得持久化 checkpoint + 恢复能力，复用第一阶段 inline checkpoint 基础设施，不引入独立 Agent 执行器。

**Architecture:** Agent 即图——`ToolUseLoop` 持有 `Arc<Graph<AgentState, AgentStateMerge>>`，ReAct 节点全部为 `ExternalLeaf`（除 `end` 为 `TaskNode`），无 Parallel/Subgraph/Barrier，通过 `validate_persistable()`。恢复校验分层：graph 泛型 helper（version/graph_hash/节点存在/步数边界）+ Agent 层（`last_response` 节点输入完整性 → `MissingExecutionContext`）。新增两个非流式入口 `invoke_with_checkpoint` / `invoke_with_restore`（trace_id 由调用方预提供 + 绑定规则），错误映射：恢复校验失败 → `LlmError::RestoreFailed{reason}`，运行期保存失败 → `LlmError::Provider`。

**Tech Stack:** Rust（workspace：lellm-core / lellm-graph / lellm-agent / lellm-provider）；serde + serde_json；tokio；tempfile（仅测试）。

**Spec:** `discuss/lellm-agent-checkpoint-phase2-design.md`（已定稿，含 4 处评审修正）。本计划从该 spec 推导；执行者应同时阅读 spec 与本计划。

## Global Constraints

- Rust 代码文件 ≤ 400 行（静态语言硬指标）。
- 每层文件夹 ≤ 8 文件；超出需规划为多层子文件夹。
- 单个测试 < 10s；涉及外部调用（HTTP/MCP/DB/进程/IO）< 30s。禁止长 `sleep`，用 `Notify`/`channel` 事件同步；禁止靠加大 timeout 掩盖慢测试。
- 优先 mock/fake，避免真实网络、服务、LLM 调用。
- `cargo fmt` 在每次提交前必跑。
- **不承诺** exactly-once / 断电 / Parallel / Subgraph / Barrier 恢复（设计 §6：工具重放被接受）。
- `LlmError` 新增变体（pre-1.0 可接受）；`RestoreFailureReason` 必须 `#[non_exhaustive]`。
- 现有 `invoke` / `invoke_stream` 行为**完全不变**（仅新增入口）。
- 简体中文注释。
- 敏感数据脱敏（前 6 位 + `****` + 后 6 位）；禁止硬编码密钥；**禁止 `rm`**（用 `mv file $HOME/.Trash/`）。
- 优先测试受影响 crate：`cargo test -p <crate>`；全部完成后再跑 workspace 全量。

## File Structure

| 文件 | 操作 | 职责 |
|------|------|------|
| `lellm-core/src/error.rs` | Modify | 新增 `RestoreFailureReason` 枚举 + `LlmError::RestoreFailed` 变体 |
| `lellm-graph/src/graph/graph_core.rs` | Modify | `run_inline_from` 拓宽为 `pub`；新增泛型 `Graph::validate_restore_checkpoint` |
| `lellm-graph/src/exec/execution_loop.rs` | Modify | `CheckpointConfig` 新增 `check_restore_latest` + `assert_fresh_trace`（store-backed，async） |
| `lellm-graph/src/test_executor.rs` | Modify | `SimpleExecutor` 委托给泛型 helper + 最新性方法（消除重复） |
| `lellm-agent/src/runtime/typed_state.rs` | Modify | `AgentCheckpoint` 新增 `last_response`；`snapshot()`/`restore()` 更新 |
| `lellm-agent/src/runtime/checkpoint.rs` | **Create** | `invoke_with_checkpoint` / `invoke_with_restore` + 辅助函数 |
| `lellm-agent/src/runtime/mod.rs` | Modify | 声明 `mod checkpoint;` |
| `lellm-agent/tests/checkpoint_restore.rs` | **Create** | 四组必测场景（Group 1~4） |
| `lellm-agent/Cargo.toml` | Modify | dev-dependencies 新增 `tempfile = "3"` |

---

### Task 1: lellm-core — `RestoreFailureReason` + `LlmError::RestoreFailed`

**Files:**
- Modify: `lellm-core/src/error.rs`（在 `LlmError` 定义前新增枚举；在 `LlmError` 变体末尾新增 `RestoreFailed`）
- Test: `lellm-core/src/error.rs`（文件底部 `#[cfg(test)] mod tests`，若无则新建）

**Interfaces:**
- Consumes: 无（core 层，无前置任务）。
- Produces:
  - `pub enum RestoreFailureReason { MissingExecutionContext, NotLatest, GraphMismatch, UnsupportedFormat, StepsExceeded, NodeNotFound, Other }`（`#[non_exhaustive]`，derive `Debug, Clone, Copy, PartialEq, Eq`，impl `Display`）。
  - `LlmError::RestoreFailed { reason: RestoreFailureReason, message: String }`。

- [ ] **Step 1: 写失败测试**

在 `lellm-core/src/error.rs` 底部新增（若已有 `mod tests` 则并入）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_failure_reason_display() {
        assert_eq!(
            RestoreFailureReason::MissingExecutionContext.to_string(),
            "missing execution context"
        );
        assert_eq!(RestoreFailureReason::NotLatest.to_string(), "not the latest checkpoint");
        assert_eq!(RestoreFailureReason::GraphMismatch.to_string(), "graph hash mismatch");
        assert_eq!(
            RestoreFailureReason::UnsupportedFormat.to_string(),
            "unsupported checkpoint format"
        );
        assert_eq!(RestoreFailureReason::StepsExceeded.to_string(), "step budget exhausted");
        assert_eq!(RestoreFailureReason::NodeNotFound.to_string(), "next node not found");
        assert_eq!(RestoreFailureReason::Other.to_string(), "other");
    }

    #[test]
    fn llm_error_restore_failed_display() {
        let e = LlmError::RestoreFailed {
            reason: RestoreFailureReason::MissingExecutionContext,
            message: "tool node requires last_response".into(),
        };
        let s = e.to_string();
        assert!(s.contains("missing execution context"), "got: {s}");
        assert!(s.contains("tool node requires last_response"), "got: {s}");
    }

    #[test]
    fn restore_failure_reason_derives() {
        let a = RestoreFailureReason::NotLatest;
        let b = a; // Copy
        let c = a.clone(); // Clone
        assert_eq!(a, b); // PartialEq
        assert_eq!(a, c); // Eq
        assert_ne!(a, RestoreFailureReason::GraphMismatch);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p lellm-core restore_failure`
Expected: 编译失败（`RestoreFailureReason` 未定义 / `LlmError::RestoreFailed` 未定义）。

- [ ] **Step 3: 实现枚举 + 变体**

在 `lellm-core/src/error.rs` 中 `LlmError` 定义**之前**新增：

```rust
/// 恢复校验失败原因分类。
///
/// `#[non_exhaustive]` — 调用方 `match` 必须带 `_` 兜底，允许后续扩展。
/// 仅表达「恢复校验失败」，不表达运行期错误（后者归 `LlmError::Provider`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RestoreFailureReason {
    /// 恢复目标节点需要 last_response 但检查点缺失（Agent 层）。
    MissingExecutionContext,
    /// 检查点不是该 trace 的最新检查点。
    NotLatest,
    /// graph_hash 不匹配（图结构已变更）。
    GraphMismatch,
    /// 检查点格式版本不支持（legacy 格式）。
    UnsupportedFormat,
    /// 步数预算已耗尽（steps_used >= max_steps）。
    StepsExceeded,
    /// next_node 在图中不存在。
    NodeNotFound,
    /// 其他未分类原因。
    Other,
}

impl std::fmt::Display for RestoreFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingExecutionContext => write!(f, "missing execution context"),
            Self::NotLatest => write!(f, "not the latest checkpoint"),
            Self::GraphMismatch => write!(f, "graph hash mismatch"),
            Self::UnsupportedFormat => write!(f, "unsupported checkpoint format"),
            Self::StepsExceeded => write!(f, "step budget exhausted"),
            Self::NodeNotFound => write!(f, "next node not found"),
            Self::Other => write!(f, "other"),
        }
    }
}
```

在 `LlmError` 枚举现有变体**末尾**（`UnexpectedEof` 之后）新增：

```rust
    /// 恢复校验失败（仅校验阶段；运行期错误归 `Provider`）。
    #[error("restore failed: {reason}: {message}")]
    RestoreFailed {
        reason: RestoreFailureReason,
        message: String,
    },
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p lellm-core`
Expected: PASS（全部 core 测试绿）。

- [ ] **Step 5: 格式化 + 提交**

```bash
cargo fmt
git add lellm-core/src/error.rs
git commit -m "feat(core): LlmError::RestoreFailed + RestoreFailureReason（#[non_exhaustive]）"
```

---

### Task 2: lellm-graph — 泛型 `Graph::validate_restore_checkpoint` + 拓宽 `run_inline_from`

**Files:**
- Modify: `lellm-graph/src/graph/graph_core.rs:338`（`run_inline_from` 由 `pub(crate)` 改 `pub`）
- Modify: `lellm-graph/src/graph/graph_core.rs:133`（`impl<S: WorkflowState, M: MergeStrategy<S>> Graph<S, M>` 块内新增 `validate_restore_checkpoint`）
- Modify: `lellm-graph/src/test_executor.rs:201,232-271`（`SimpleExecutor` 委托给泛型 helper，删除私有重复实现）
- Test: `lellm-graph/src/graph/graph_core.rs`（文件底部 `#[cfg(test)] mod tests`，若无则新建）

**Interfaces:**
- Consumes: `Checkpoint<S>`（`crate::checkpoint::Checkpoint`）、`GraphError`/`TerminalError`（已 import）、`node_map()`（`pub`，graph_core.rs:200）、`canonical_hash()`（`pub`，graph_core.rs:154）。
- Produces:
  - `pub fn validate_restore_checkpoint(&self, cp: &Checkpoint<S>, max_steps: usize) -> Result<(), GraphError>`（挂在 `Graph<S, M>` 上）。
  - `run_inline_from` 变为 `pub`（签名不变）。

- [ ] **Step 1: 写失败测试**

在 `lellm-graph/src/graph/graph_core.rs` 底部新增（文件需已有 `use` 或在此处局部引入）：

```rust
#[cfg(test)]
mod restore_validation_tests {
    use super::*;
    use crate::checkpoint::{Checkpoint, CheckpointId, CHECKPOINT_FORMAT_VERSION, NodeId};
    use crate::error::TerminalError;
    use crate::{GraphBuilder, NodeKind, State, StateMerge, TaskNode};
    use std::time::SystemTime;

    fn single_node_graph() -> Graph<State, StateMerge> {
        let mut b = GraphBuilder::<State, StateMerge>::new("t");
        b.start("a");
        b.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        b.end("a");
        b.build().expect("build")
    }

    fn cp(next: Option<NodeId>, hash: u64, steps: usize) -> Checkpoint<State> {
        Checkpoint {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: CheckpointId::new(),
            next_node: next,
            state: State::new(),
            graph_hash: hash,
            steps_used: steps,
            created_at: SystemTime::now(),
        }
    }

    #[test]
    fn valid_checkpoint_passes() {
        let g = single_node_graph();
        let h = g.canonical_hash();
        assert!(g.validate_restore_checkpoint(&cp(Some(NodeId("a".into())), h, 0), 10).is_ok());
    }

    #[test]
    fn graph_hash_mismatch_rejected() {
        let g = single_node_graph();
        let h = g.canonical_hash();
        let err = g.validate_restore_checkpoint(&cp(Some(NodeId("a".into())), h ^ 0xff, 0), 10)
            .unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::RestoreFailed { .. })
        ));
    }

    #[test]
    fn node_not_found_rejected() {
        let g = single_node_graph();
        let h = g.canonical_hash();
        let err = g
            .validate_restore_checkpoint(&cp(Some(NodeId("nope".into())), h, 0), 10)
            .unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::NodeNotFound(_))
        ));
    }

    #[test]
    fn steps_exhausted_with_next_rejected() {
        let g = single_node_graph();
        let h = g.canonical_hash();
        let err = g
            .validate_restore_checkpoint(&cp(Some(NodeId("a".into())), h, 10), 10)
            .unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::StepsExceeded { .. })
        ));
    }

    #[test]
    fn completed_with_exhausted_budget_allowed() {
        let g = single_node_graph();
        let h = g.canonical_hash();
        // next_node=None（完成态）允许预算耗尽
        assert!(g.validate_restore_checkpoint(&cp(None, h, 10), 10).is_ok());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p lellm-graph restore_validation`
Expected: 编译失败（`validate_restore_checkpoint` 未定义）。

- [ ] **Step 3: 实现泛型 helper + 拓宽可见性**

在 `lellm-graph/src/graph/graph_core.rs` 顶部 imports 区（第 18 行 `use crate::error::{...}` 附近）补充 `Checkpoint`：

```rust
use crate::checkpoint::Checkpoint;
```

将 `run_inline_from`（第 338 行）的 `pub(crate)` 改为 `pub`：

```rust
    /// 统一执行入口 — 首次运行与恢复共用。
    ///
    /// - `start_node` — 首次运行 = `start_node()`；恢复 = `checkpoint.next_node`
    /// - `steps_used` — 首次运行 = 0；恢复 = `checkpoint.steps_used`（预算延续）
    /// - `max_steps` — **总预算**（恢复时同样传总预算）
    pub async fn run_inline_from<'cb>(
        &self,
        exec_ctx: &mut ExecutionEngine<'_, S>,
        start_node: &str,
        steps_used: usize,
        max_steps: usize,
        step_cb: &mut dyn StepCallback<'cb>,
    ) -> Result<(), GraphError> {
        crate::graph::run_loop::run_graph_loop(
            self, exec_ctx, start_node, steps_used, max_steps, step_cb,
        )
        .await
    }
```

在 `impl<S: WorkflowState, M: MergeStrategy<S>> Graph<S, M>` 块内（`validate_persistable` 之后，第 374 行 `}` 之前）新增：

```rust
    /// 恢复入口同步校验（版本 / 指纹 / 节点存在 / 步数边界）。
    ///
    /// 泛型版 — 供 `SimpleExecutor`（`State`）与 Agent 层（`AgentState`）共用。
    /// 只做图结构层面校验，不识别具体 State 类型 / 节点语义
    /// （节点输入完整性由上层，如 Agent 的 last_response 校验，负责）。
    pub fn validate_restore_checkpoint(
        &self,
        cp: &Checkpoint<S>,
        max_steps: usize,
    ) -> Result<(), GraphError> {
        if cp.format_version != crate::checkpoint::CHECKPOINT_FORMAT_VERSION {
            return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                reason: format!(
                    "unsupported checkpoint format_version: {} (expected {})",
                    cp.format_version,
                    crate::checkpoint::CHECKPOINT_FORMAT_VERSION
                ),
            }));
        }
        if cp.graph_hash != self.canonical_hash() {
            return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                reason: format!(
                    "graph hash mismatch: expected {:016x}, got {:016x}",
                    self.canonical_hash(),
                    cp.graph_hash
                ),
            }));
        }
        if let Some(n) = &cp.next_node {
            if !self.node_map().contains_key(&n.0) {
                return Err(GraphError::Terminal(TerminalError::NodeNotFound(n.0.clone())));
            }
            // 还有下一节点但预算已耗尽 → 执行前报错（完成态除外）
            if cp.steps_used >= max_steps {
                return Err(GraphError::Terminal(TerminalError::StepsExceeded {
                    limit: max_steps,
                }));
            }
        }
        // next_node = None（完成态）：允许预算耗尽（零执行直接返回完成）
        Ok(())
    }
```

- [ ] **Step 4: 重构 `SimpleExecutor` 委托给泛型 helper**

在 `lellm-graph/src/test_executor.rs` 中，将第 201 行：

```rust
        Self::validate_restore_checkpoint(&graph, &restore_from, self.max_steps)?;
```

改为：

```rust
        graph.validate_restore_checkpoint(&restore_from, self.max_steps)?;
```

删除第 232-271 行的私有 `fn validate_restore_checkpoint(graph: &Graph, ...)` 整个函数（含 doc 注释）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p lellm-graph`
Expected: PASS（`restore_validation` 新测试绿 + 既有 T1-T10 / restore 测试全绿，无回归）。

- [ ] **Step 6: 格式化 + 提交**

```bash
cargo fmt
git add lellm-graph/src/graph/graph_core.rs lellm-graph/src/test_executor.rs
git commit -m "refactor(graph): 泛型 validate_restore_checkpoint + run_inline_from 拓宽为 pub"
```

---

### Task 3: lellm-graph — `CheckpointConfig::check_restore_latest` + `assert_fresh_trace`

**Files:**
- Modify: `lellm-graph/src/exec/execution_loop.rs:28-125`（`impl<S: WorkflowState> CheckpointConfig<S>` 块内新增两个 async 方法）
- Modify: `lellm-graph/src/exec/execution_loop.rs:14`（imports 补充 `GraphError`/`TerminalError`）
- Modify: `lellm-graph/src/test_executor.rs:203-226`（`SimpleExecutor` 最新性检查委托给 `check_restore_latest`）
- Test: `lellm-graph/src/exec/execution_loop.rs`（文件底部 `#[cfg(test)] mod tests`，若无则新建）

**Interfaces:**
- Consumes: `self.store: Option<Arc<dyn BlobCheckpointStore>>`（`pub(crate)`）、`store.load_latest(&TraceId) -> Result<Option<CheckpointBlob>, CheckpointStoreError>`、`Checkpoint<S>.checkpoint_id`、`TerminalError::RestoreNotLatest{checkpoint,latest}` / `RestoreFailed{reason}`。
- Produces:
  - `pub async fn check_restore_latest(&self, trace_id: &TraceId, cp: &Checkpoint<S>) -> Result<(), GraphError>`（store 不存在时跳过）。
  - `pub async fn assert_fresh_trace(&self, trace_id: &TraceId) -> Result<(), GraphError>`（store 不存在时跳过；已有检查点 → `RestoreFailed`）。

- [ ] **Step 1: 写失败测试**

在 `lellm-graph/src/exec/execution_loop.rs` 底部新增（文件需已有 `use` 或在此处局部引入）：

```rust
#[cfg(test)]
mod restore_latestness_tests {
    use super::*;
    use crate::checkpoint::{Checkpoint, CheckpointId, NodeId, CHECKPOINT_FORMAT_VERSION};
    use crate::error::TerminalError;
    use crate::{GraphBuilder, InMemoryBlobStore, NodeKind, SerdeCheckpointCodec, State, StateMerge, TaskNode};
    use std::sync::Arc;
    use std::time::SystemTime;

    fn single_node_graph() -> (crate::Graph, u64) {
        let mut b = GraphBuilder::<State, StateMerge>::new("t");
        b.start("a");
        b.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        b.end("a");
        let g = b.build().expect("build");
        (g, g.canonical_hash())
    }

    fn cp(id: CheckpointId, hash: u64) -> Checkpoint<State> {
        Checkpoint {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: id,
            next_node: Some(NodeId("a".into())),
            state: State::new(),
            graph_hash: hash,
            steps_used: 0,
            created_at: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn check_restore_latest_accepts_latest() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let tid = crate::TraceId::new();
        let id = CheckpointId::new();
        let blob = codec.serialize(&cp(id, h), h).unwrap();
        store.save_with_trace(&tid, &blob).await.unwrap();

        let cfg = CheckpointConfig::for_store(store, codec, h);
        assert!(cfg.check_restore_latest(&tid, &cp(id, h)).await.is_ok());
    }

    #[tokio::test]
    async fn check_restore_latest_rejects_stale() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let tid = crate::TraceId::new();
        let stale_id = CheckpointId::new();
        let latest_id = CheckpointId::new();
        // 先存 stale，再存 latest（latest 成为 load_latest 结果）
        let b1 = codec.serialize(&cp(stale_id, h), h).unwrap();
        store.save_with_trace(&tid, &b1).await.unwrap();
        let b2 = codec.serialize(&cp(latest_id, h), h).unwrap();
        store.save_with_trace(&tid, &b2).await.unwrap();

        let cfg = CheckpointConfig::for_store(store, codec, h);
        let err = cfg.check_restore_latest(&tid, &cp(stale_id, h)).await.unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::RestoreNotLatest { .. })
        ));
    }

    #[tokio::test]
    async fn check_restore_latest_no_store_skips() {
        let (_g, h) = single_node_graph();
        let cfg = CheckpointConfig::new(
            |_c: Checkpoint<State>, _t: crate::TraceId| {
                Box::pin(async { Ok::<(), crate::checkpoint::CheckpointStoreError>(()) })
            },
            h,
        );
        // store=None → 跳过最新性校验
        assert!(cfg.check_restore_latest(&crate::TraceId::new(), &cp(CheckpointId::new(), h)).await.is_ok());
    }

    #[tokio::test]
    async fn assert_fresh_trace_rejects_existing() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let tid = crate::TraceId::new();
        let blob = codec.serialize(&cp(CheckpointId::new(), h), h).unwrap();
        store.save_with_trace(&tid, &blob).await.unwrap();

        let cfg = CheckpointConfig::for_store(store, codec, h);
        let err = cfg.assert_fresh_trace(&tid).await.unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::RestoreFailed { .. })
        ));
    }

    #[tokio::test]
    async fn assert_fresh_trace_accepts_empty() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let cfg = CheckpointConfig::for_store(store, codec, h);
        assert!(cfg.assert_fresh_trace(&crate::TraceId::new()).await.is_ok());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p lellm-graph restore_latestness`
Expected: 编译失败（`check_restore_latest` / `assert_fresh_trace` 未定义）。

- [ ] **Step 3: 实现两个方法**

在 `lellm-graph/src/exec/execution_loop.rs` 顶部 imports（第 14 行 `use crate::checkpoint::{Checkpoint, CheckpointSink, TraceId};` 附近）补充：

```rust
use crate::error::{GraphError, TerminalError};
```

在 `impl<S: WorkflowState> CheckpointConfig<S>` 块内（`apply_retention` 之后，第 124 行 `}` 之前）新增：

```rust
    /// 恢复最新性校验 — 传入检查点必须是该 trace 的最新检查点。
    ///
    /// `store` 不存在时跳过（无持久化 → 无最新性概念）。
    /// 只接受该 trace 的最新检查点并续写原 trace，避免未定义的历史分叉。
    pub async fn check_restore_latest(
        &self,
        trace_id: &TraceId,
        cp: &Checkpoint<S>,
    ) -> Result<(), GraphError> {
        if let Some(store) = &self.store {
            match store.load_latest(trace_id).await {
                Ok(Some(latest)) => {
                    if latest.id != cp.checkpoint_id {
                        return Err(GraphError::Terminal(TerminalError::RestoreNotLatest {
                            checkpoint: cp.checkpoint_id.to_string(),
                            latest: latest.id.to_string(),
                        }));
                    }
                }
                Ok(None) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!("no checkpoints found for trace {trace_id}"),
                    }));
                }
                Err(e) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!("load latest checkpoint: {e}"),
                    }));
                }
            }
        }
        Ok(())
    }

    /// 首次执行新鲜度校验 — 要求该 trace 无既有检查点。
    ///
    /// `store` 不存在时跳过（无持久化 → 无既有检查点）。
    /// 已有检查点 → 报错（提示改用恢复入口），避免新状态接在旧执行历史后。
    pub async fn assert_fresh_trace(&self, trace_id: &TraceId) -> Result<(), GraphError> {
        if let Some(store) = &self.store {
            match store.load_latest(trace_id).await {
                Ok(Some(latest)) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!(
                            "trace {trace_id} already has checkpoint {} (use restore entry)",
                            latest.id
                        ),
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!("load latest checkpoint: {e}"),
                    }));
                }
            }
        }
        Ok(())
    }
```

- [ ] **Step 4: 重构 `SimpleExecutor` 最新性检查委托**

在 `lellm-graph/src/test_executor.rs` 中，将第 203-226 行的内联最新性检查（`if let Some(store) = &config.store { ... }` 整块）替换为：

```rust
        // 最新性检查（需 store I/O → async）：只接受该 trace 最新检查点并续写原 trace
        config.check_restore_latest(&trace_id, &restore_from).await?;
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p lellm-graph`
Expected: PASS（`restore_latestness` 新测试绿 + 既有 T1-T10 / restore 测试全绿，无回归）。

- [ ] **Step 6: 格式化 + 提交**

```bash
cargo fmt
git add lellm-graph/src/exec/execution_loop.rs lellm-graph/src/test_executor.rs
git commit -m "feat(graph): CheckpointConfig::check_restore_latest + assert_fresh_trace（store-backed）"
```

---

### Task 4: lellm-agent — `AgentCheckpoint.last_response`（Pending Context）

**Files:**
- Modify: `lellm-agent/src/runtime/typed_state.rs`（`AgentCheckpoint` 结构体新增字段；`snapshot()` 包含它；`restore()` 恢复它）
- Test: `lellm-agent/src/runtime/typed_state.rs`（文件底部 `#[cfg(test)] mod tests`，若无则新建）

**Interfaces:**
- Consumes: `AgentState.last_response: Option<ChatResponse>`（运行时字段，已存在）、`ChatResponse`（`Serialize + Deserialize`）。
- Produces:
  - `AgentCheckpoint { ..., last_response: Option<ChatResponse> }`（新增字段）。
  - `AgentState::snapshot()` 现在把 `last_response` 写入 `AgentCheckpoint`。
  - `AgentState::restore(cp)` 现在把 `cp.last_response` 恢复回 `AgentState.last_response`。

- [ ] **Step 1: 写失败测试**

在 `lellm-agent/src/runtime/typed_state.rs` 底部新增：

```rust
#[cfg(test)]
mod checkpoint_last_response_tests {
    use super::*;
    use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage};

    fn response_with(text: &str) -> ChatResponse {
        ChatResponse::new(
            lellm_core::text_block(text),
            TokenUsage::default(),
            serde_json::Value::Null,
        )
    }

    #[test]
    fn snapshot_preserves_last_response() {
        let mut state = AgentState::from_messages(vec![Message::user_text("q")]);
        state.last_response = Some(response_with("answer"));
        let cp = state.snapshot();
        assert!(cp.last_response.is_some(), "snapshot 应包含 last_response");
        let restored = AgentState::restore(cp);
        assert!(restored.last_response.is_some(), "restore 应恢复 last_response");
        assert_eq!(
            ContentBlock::flatten_text(&restored.last_response.as_ref().unwrap().content),
            "answer"
        );
    }

    #[test]
    fn snapshot_none_stays_none() {
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let cp = state.snapshot();
        assert!(cp.last_response.is_none(), "初始状态 last_response=None");
        let restored = AgentState::restore(cp);
        assert!(restored.last_response.is_none());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p lellm-agent checkpoint_last_response`
Expected: 编译失败（`AgentCheckpoint` 无 `last_response` 字段）。

- [ ] **Step 3: 实现字段 + snapshot/restore**

在 `lellm-agent/src/runtime/typed_state.rs` 中：

1. `AgentCheckpoint` 结构体（约 170-186 行）新增字段（替换原 `// 不包含: last_response（可重建）` 注释）：

```rust
    /// Pending Context — 最近一次 LLM 响应（恢复时 post_llm_check / tool 节点需要）。
    ///
    /// 旧检查点（本字段加入前创建）反序列化为 `None` → 完成态结果从 messages 重建（§5.4）。
    #[serde(default)]
    pub last_response: Option<lellm_core::ChatResponse>,
```

2. `AgentState` 的 `WorkflowState` impl 中 `snapshot()`（约 194-204 行）：在构造 `AgentCheckpoint` 时加入 `last_response: self.last_response.clone(),`。

3. 同 impl 中 `restore(checkpoint)`（约 206-217 行）：把 `last_response: None` 改为 `last_response: checkpoint.last_response,`。

> 注意：`#[serde(default)]` 保证旧检查点（无 `last_response` 键）反序列化为 `None`，不破坏向后兼容。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p lellm-agent`
Expected: PASS（`checkpoint_last_response` 新测试绿 + 既有 agent 测试全绿）。

- [ ] **Step 5: 格式化 + 提交**

```bash
cargo fmt
git add lellm-agent/src/runtime/typed_state.rs
git commit -m "feat(agent): AgentCheckpoint 新增 last_response（Pending Context，#[serde(default)]）"
```

---

### Task 5: lellm-agent — `invoke_with_checkpoint` / `invoke_with_restore` + Group 1

**Files:**
- Create: `lellm-agent/src/runtime/checkpoint.rs`（两个入口 + 辅助函数；`validate_last_response` 本任务为**桩**，Task 6 落实）
- Modify: `lellm-agent/src/runtime/mod.rs`（声明 `mod checkpoint;`）
- Create: `lellm-agent/tests/checkpoint_restore.rs`（测试基建 + Group 1a/1b）
- Modify: `lellm-agent/Cargo.toml`（dev-dependencies 新增 `tempfile = "3"`）

**Interfaces:**
- Consumes:
  - Task 1: `LlmError::RestoreFailed`、`RestoreFailureReason`。
  - Task 2: `Graph::validate_restore_checkpoint`、`run_inline_from`（`pub`）。
  - Task 3: `CheckpointConfig::check_restore_latest` / `assert_fresh_trace`。
  - Task 4: `AgentCheckpoint.last_response`、`snapshot()`/`restore()`。
  - `lellm_graph::{Checkpoint, CheckpointConfig, ExecutionContext, NoopStepCallback, CancellationToken, TraceId, GraphError, TerminalError}`、`lellm_graph::exec::CheckpointSaveSink`、`lellm_graph::{InMemoryBlobStore, FileBlobStore, SerdeCheckpointCodec, NodeId, CheckpointStoreError, BlobCheckpointStore}`。
  - `super::config::{ToolUseConfig, build_request_messages_inner, empty_response}`、`super::event::StopReason`、`super::typed_state::AgentState`、`super::runtime::ToolUseLoop`、`super::ToolUseResult`。
- Produces:
  - `ToolUseLoop::invoke_with_checkpoint(&self, messages: Vec<Message>, trace_id: TraceId, config: CheckpointConfig<AgentState>) -> Result<ToolUseResult, LlmError>`。
  - `ToolUseLoop::invoke_with_restore(&self, checkpoint: Checkpoint<AgentState>, trace_id: TraceId, config: CheckpointConfig<AgentState>) -> Result<ToolUseResult, LlmError>`。
  - 辅助函数（`checkpoint.rs` 内，`pub(crate)`）：`max_steps_for`、`build_result`、`reconstruct_response_from_messages`、`validate_last_response`（本任务桩）、`map_restore_error`、`map_runtime_error`。

- [ ] **Step 1: 添加 `tempfile` dev-dependency**

在 `lellm-agent/Cargo.toml` 的 `[dev-dependencies]` 块内（`schemars.workspace = true` 之后）新增：

```toml
tempfile = "3"
```

- [ ] **Step 2: 写 Group 1 失败测试（含测试基建）**

创建 `lellm-agent/tests/checkpoint_restore.rs`：

```rust
//! Agent Checkpoint Phase 2 — 四组必测场景。
//!
//! 崩溃模拟：save_fn 在 `next_node` 命中目标时拒绝保存并报错，
//! 执行在该边界停止（不越过），前一检查点成为 latest（复刻设计 §6 崩溃窗口）。
//! 工具重放被接受（不承诺 exactly-once）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::stream;
use lellm_agent::{AgentBuilder, AgentState, ExecutableTool, ResolvedModel, StopReason};
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, RestoreFailureReason, TokenUsage,
    ToolCall, ToolDefinition, ToolSchema,
};
use lellm_graph::{
    Checkpoint, CheckpointConfig, CheckpointStoreError, FileBlobStore, InMemoryBlobStore, NodeId,
    SerdeCheckpointCodec, TraceId,
};
use lellm_provider::{LlmProvider, ProviderEvent, ProviderStream};

// ─── 脚本化 Provider（MockProvider 只能返回首个；此处按序弹出）──────────────

struct ScriptedProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<ChatResponse>>,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses.into()),
            call_count: AtomicUsize::new(0),
        }
    }
    fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
    fn next(&self) -> Result<ChatResponse, LlmError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.responses.lock().unwrap().pop_front().ok_or(LlmError::Provider {
            provider: "scripted".into(),
            status: Some(500),
            code: None,
            message: "no scripted response left".into(),
        })
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    async fn call(&self, _request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.next()
    }
    async fn stream(&self, _request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        let response = self.next()?;
        let events: Vec<Result<ProviderEvent, LlmError>> = vec![
            Ok(ProviderEvent::Start { model: String::new() }),
            Ok(ProviderEvent::Token {
                token: response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            }),
            Ok(ProviderEvent::ResponseComplete {
                tool_calls: response.tool_calls().cloned().collect(),
                usage: Some(response.usage),
            }),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
    fn provider_id(&self) -> &str {
        "scripted"
    }
}

// ─── 测试工具 ─────────────────────────────────────────────────────

fn tool_call_response() -> ChatResponse {
    ChatResponse::new(
        vec![ContentBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "calc".into(),
            arguments: serde_json::json!({"expr": "6*7"}),
        })],
        TokenUsage::default(),
        serde_json::Value::Null,
    )
}

fn text_response(text: &str) -> ChatResponse {
    ChatResponse::new(
        lellm_core::text_block(text),
        TokenUsage::default(),
        serde_json::Value::Null,
    )
}

fn make_tool(counter: Arc<AtomicUsize>) -> ExecutableTool {
    let def = ToolDefinition {
        name: "calc".to_string(),
        description: "calculator".to_string(),
        parameters: ToolSchema::new(serde_json::json!({
            "type": "object",
            "properties": { "expr": { "type": "string" } }
        })),
        cache_control: None,
    };
    ExecutableTool::safe(def, move |_args: &serde_json::Value| {
        let c = counter.clone();
        async move {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!("42"))
        }
    })
}

/// 构造「在 next_node 命中 target 时拒绝保存」的 config（崩溃模拟）。
fn crash_config(
    store: Arc<InMemoryBlobStore>,
    codec: SerdeCheckpointCodec<AgentState>,
    hash: u64,
    target: &str,
) -> CheckpointConfig<AgentState> {
    let t = target.to_string();
    CheckpointConfig::new(
        move |cp: Checkpoint<AgentState>, tid: TraceId| {
            let s = store.clone();
            let c = codec.clone();
            let h = hash;
            let t = t.clone();
            Box::pin(async move {
                if cp.next_node.as_ref().map(|n| n.0.as_str()) == Some(t.as_str()) {
                    return Err(CheckpointStoreError::Storage("simulated crash".into()));
                }
                let blob = c.serialize(&cp, h)?;
                s.save_with_trace(&tid, &blob).await
            })
        },
        hash,
    )
    .with_store(store)
}

// ─── Group 1: 恢复正确性 ─────────────────────────────────────────

/// G1a: 崩溃在 tool 后（latest = next=tool）→ 恢复执行工具、不重调已完成的 LLM。
#[tokio::test]
async fn g1a_restore_at_tool_executes_tool_no_rellm() {
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("answer: 42"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(5)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();

    let hash = agent.graph().canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=budget_check（tool 执行后）→ latest = next=tool
    let config_a = crash_config(store.clone(), codec.clone(), hash, "budget_check");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("what is 6*7?")], trace_id, config_a)
        .await;
    assert!(result_a.is_err(), "phase A 应在 tool 边界崩溃");

    // 最新检查点 = next=tool，last_response 已入 checkpoint
    let blob = store.load_latest(&trace_id).await.unwrap().expect("latest");
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("tool".into())));
    assert!(cp.state.last_response.is_some(), "last_response 必须已入 checkpoint");

    // 阶段 B：恢复（正常 config）
    tool_calls.store(0, Ordering::SeqCst); // 只统计恢复后的工具执行
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent.invoke_with_restore(cp, trace_id, config_b).await.unwrap();

    let answer = ContentBlock::flatten_text(&result_b.response.content);
    assert!(answer.contains("42"), "最终回答存在, got: {answer}");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1, "恢复后工具执行一次");
    assert_eq!(provider.call_count(), 2, "LLM 恰好两次（恢复后未重调已完成的 LLM）");
    assert_eq!(result_b.stop_reason, StopReason::Complete);
}

/// G1b: 崩溃在 post_llm_check 前（latest = next=post_llm_check）→ PostLLMGuard 正确读取 last_response 路由到 tool（而非误判 Complete）。
#[tokio::test]
async fn g1b_restore_at_post_llm_check_guard_reads_last_response() {
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("done"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(5)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();

    let hash = agent.graph().canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=tool → latest = next=post_llm_check（post_llm_check 未执行）
    let config_a = crash_config(store.clone(), codec.clone(), hash, "tool");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], trace_id, config_a)
        .await;
    assert!(result_a.is_err());

    let blob = store.load_latest(&trace_id).await.unwrap().expect("latest");
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("post_llm_check".into())));
    assert!(cp.state.last_response.is_some());

    // 阶段 B：恢复 — 若 last_response 丢失，PostLLMGuard 会误判 Complete（tool 不执行）
    tool_calls.store(0, Ordering::SeqCst);
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent.invoke_with_restore(cp, trace_id, config_b).await.unwrap();

    assert_eq!(
        tool_calls.load(Ordering::SeqCst),
        1,
        "PostLLMGuard 读取 last_response 路由到 tool（非 Complete）"
    );
    assert!(ContentBlock::flatten_text(&result_b.response.content).contains("done"));
    assert_eq!(provider.call_count(), 2);
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p lellm-agent --test checkpoint_restore`
Expected: 编译失败（`invoke_with_checkpoint` / `invoke_with_restore` 未定义）。

- [ ] **Step 4: 实现 `checkpoint.rs`（`validate_last_response` 为桩）**

创建 `lellm-agent/src/runtime/checkpoint.rs`：

```rust
//! Agent Checkpoint Phase 2 — 持久化执行 + 恢复入口。
//!
//! 复用第一阶段 inline checkpoint 基础设施：
//! - `invoke_with_checkpoint` — 首次执行（trace 必须新鲜）。
//! - `invoke_with_restore` — 从检查点恢复（分层校验 + 续跑）。
//!
//! 错误映射：恢复**校验**失败 → `LlmError::RestoreFailed{reason}`；
//! 运行期错误（含保存失败）→ `LlmError::Provider`。

use lellm_core::{ChatResponse, ContentBlock, LlmError, Message, RestoreFailureReason, TokenUsage};
use lellm_graph::exec::CheckpointSaveSink;
use lellm_graph::{
    Checkpoint, CheckpointConfig, CancellationToken, ExecutionContext, GraphError,
    NoopStepCallback, TerminalError, TraceId,
};

use super::config::{ToolUseConfig, build_request_messages_inner, empty_response};
use super::event::StopReason;
use super::runtime::ToolUseLoop;
use super::typed_state::AgentState;
use super::ToolUseResult;

// ─── 辅助函数 ───────────────────────────────────────────────────

/// 图步数预算 — 与 `invoke` 一致（每轮 ReAct 最坏 4 steps + 1 buffer）。
pub(crate) fn max_steps_for(config: &ToolUseConfig) -> usize {
    config.max_iterations * 4 + 1
}

/// 从 AgentState 构造 ToolUseResult。
///
/// 完成态结果构造契约（§5.4）：`last_response` 缺失（旧检查点）时
/// 从 messages 的最后一条 Assistant 消息重建 response。
pub(crate) fn build_result(state: &AgentState) -> ToolUseResult {
    let stop_reason = state.stop_reason.clone().unwrap_or(StopReason::Complete);
    let response = state
        .last_response
        .clone()
        .unwrap_or_else(|| reconstruct_response_from_messages(&state.messages));
    ToolUseResult {
        stop_reason,
        response,
        messages: state.messages.clone(),
        iterations: state.iterations,
        tool_calls_executed: state.total_tool_calls,
    }
}

/// 从 messages 的最后一条 Assistant 消息重建 ChatResponse（§5.4）。
pub(crate) fn reconstruct_response_from_messages(messages: &[Message]) -> ChatResponse {
    match messages.iter().rev().find_map(|m| match m {
        Message::Assistant { content } => Some(content.clone()),
        _ => None,
    }) {
        Some(content) => {
            let text = ContentBlock::flatten_text(&content);
            ChatResponse::new(
                lellm_core::text_block(text),
                TokenUsage::default(),
                serde_json::Value::Null,
            )
        }
        None => empty_response(),
    }
}

/// Agent 层校验 — 恢复目标节点对 last_response 的输入完整性（§5.2）。
///
/// `post_llm_check` / `tool` 需要 `last_response = Some`，否则 `MissingExecutionContext`；
/// `budget_check` / `llm` / `compactor` / `end` / `None` 允许 `None`。
///
/// **Task 5 桩** — 本任务返回 `Ok(())`；Task 6 落实真实校验。
pub(crate) fn validate_last_response(
    _cp: &Checkpoint<AgentState>,
) -> Result<(), LlmError> {
    Ok(())
}

/// 恢复**校验**阶段错误 → `LlmError::RestoreFailed{reason}`。
///
/// 承接 4 个校验步骤：`validate_persistable`（`RestoreUnsupported` → catch-all → `Other`）、
/// `validate_restore_checkpoint`、`check_restore_latest`、`validate_last_response`。
pub(crate) fn map_restore_error(e: GraphError) -> LlmError {
    match e {
        GraphError::Terminal(TerminalError::RestoreFailed { reason }) => {
            let r = if reason.contains("format_version") {
                RestoreFailureReason::UnsupportedFormat
            } else if reason.contains("hash mismatch") {
                RestoreFailureReason::GraphMismatch
            } else {
                RestoreFailureReason::Other
            };
            LlmError::RestoreFailed {
                reason: r,
                message: reason,
            }
        }
        GraphError::Terminal(TerminalError::RestoreNotLatest {
            checkpoint,
            latest,
        }) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::NotLatest,
            message: format!("checkpoint {checkpoint} is not the latest ({latest})"),
        },
        GraphError::Terminal(TerminalError::NodeNotFound(name)) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::NodeNotFound,
            message: format!("next node not found: {name}"),
        },
        GraphError::Terminal(TerminalError::StepsExceeded { limit }) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::StepsExceeded,
            message: format!("step budget exhausted (limit {limit})"),
        },
        GraphError::Terminal(other) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::Other,
            message: format!("{other:?}"),
        },
    }
}

/// 运行期错误（含保存失败）→ `LlmError::Provider`（非恢复失败）。
pub(crate) fn map_runtime_error(e: GraphError) -> LlmError {
    LlmError::Provider {
        provider: "react_graph".into(),
        status: None,
        code: None,
        message: e.to_string(),
    }
}

// ─── ToolUseLoop 入口 ───────────────────────────────────────────

impl ToolUseLoop {
    /// 非流式首次执行（带 checkpoint 保存）。
    ///
    /// `trace_id` 由调用方预提供；要求该 trace **无既有检查点**（新鲜），
    /// 否则报错提示改用 `invoke_with_restore`。
    pub async fn invoke_with_checkpoint(
        &self,
        messages: Vec<Message>,
        trace_id: TraceId,
        config: CheckpointConfig<AgentState>,
    ) -> Result<ToolUseResult, LlmError> {
        // 首次执行绑定规则：trace 必须新鲜
        config.assert_fresh_trace(&trace_id).await.map_err(|e| {
            LlmError::InvalidRequest {
                message: format!(
                    "trace {trace_id} already has checkpoints; use invoke_with_restore: {e}"
                ),
            }
        })?;

        let initial_messages = build_request_messages_inner(&self.config, &messages)?;
        let max_steps = max_steps_for(&self.config);
        let mut state = AgentState::from_messages(initial_messages);

        let mut sink = CheckpointSaveSink::new(config, trace_id, None);
        let mut step_cb = NoopStepCallback;
        let mut engine = ExecutionContext::new(
            &mut state,
            None,
            CancellationToken::new(),
            Some(&mut sink),
            None,
        );
        self.graph
            .run_inline(&mut engine, max_steps, &mut step_cb)
            .await
            .map_err(map_runtime_error)?;
        // 经 engine 取状态（与 invoke 一致，避免与 engine 持有的 &mut state 冲突）
        Ok(build_result(engine.state()))
    }

    /// 非流式恢复执行 — 从检查点续跑，继续保存到同一 trace。
    ///
    /// 分层校验：
    /// 1. 图可持久化（`validate_persistable`）
    /// 2. Graph 泛型校验（版本/指纹/节点存在/步数边界）
    /// 3. 最新性（`check_restore_latest`，store-backed）
    /// 4. Agent 层校验（`validate_last_response`）
    /// 完成态（`next_node = None`）零执行，直接从 state 构造结果（§5.4）。
    pub async fn invoke_with_restore(
        &self,
        checkpoint: Checkpoint<AgentState>,
        trace_id: TraceId,
        config: CheckpointConfig<AgentState>,
    ) -> Result<ToolUseResult, LlmError> {
        let max_steps = max_steps_for(&self.config);
        // 恢复校验层（4 步）— 全部映射 RestoreFailed：
        // ① 图可持久化（无 Parallel/Subgraph/Barrier）
        self.graph
            .validate_persistable()
            .map_err(map_restore_error)?;
        // ② Graph 泛型校验（版本/指纹/节点存在/步数边界）
        self.graph
            .validate_restore_checkpoint(&checkpoint, max_steps)
            .map_err(map_restore_error)?;
        config
            .check_restore_latest(&trace_id, &checkpoint)
            .await
            .map_err(map_restore_error)?;
        validate_last_response(&checkpoint)?;

        let mut state = checkpoint.restore_state();

        // 完成态：零执行，直接从 state 构造结果（§5.4 契约）
        let next_node = match checkpoint.next_node {
            Some(n) => n,
            None => return Ok(build_result(&state)),
        };

        let mut sink = CheckpointSaveSink::new(config, trace_id, None);
        let mut step_cb = NoopStepCallback;
        let mut engine = ExecutionContext::new(
            &mut state,
            None,
            CancellationToken::new(),
            Some(&mut sink),
            None,
        );
        self.graph
            .run_inline_from(
                &mut engine,
                &next_node.0,
                checkpoint.steps_used,
                max_steps,
                &mut step_cb,
            )
            .await
            .map_err(map_runtime_error)?;
        // 经 engine 取状态（与 invoke 一致，避免与 engine 持有的 &mut state 冲突）
        Ok(build_result(engine.state()))
    }
}
```

在 `lellm-agent/src/runtime/mod.rs` 中声明模块（在 `mod config;` 附近，按字母序）：

```rust
mod checkpoint;
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p lellm-agent --test checkpoint_restore`
Expected: PASS（G1a + G1b 绿）。

Run: `cargo test -p lellm-agent`
Expected: PASS（既有 agent 测试全绿，无回归）。

- [ ] **Step 6: 格式化 + 提交**

```bash
cargo fmt
git add lellm-agent/src/runtime/checkpoint.rs lellm-agent/src/runtime/mod.rs lellm-agent/tests/checkpoint_restore.rs lellm-agent/Cargo.toml
git commit -m "feat(agent): invoke_with_checkpoint/invoke_with_restore + Group 1 恢复正确性测试"
```

---

### Task 6: lellm-agent — Agent 层 `validate_last_response` + Group 3

**Files:**
- Modify: `lellm-agent/src/runtime/checkpoint.rs`（`validate_last_response` 桩 → 真实校验）
- Test: `lellm-agent/tests/checkpoint_restore.rs`（新增 Group 3）

**Interfaces:**
- Consumes: Task 5 的 `invoke_with_restore`（已调用 `validate_last_response`）、`Checkpoint<AgentState>.next_node` / `.state.last_response`。
- Produces: `validate_last_response` 真实逻辑 — `post_llm_check`/`tool` 且 `last_response = None` → `LlmError::RestoreFailed{reason: MissingExecutionContext}`；其余节点允许 `None`。

- [ ] **Step 1: 写 Group 3 失败测试**

在 `lellm-agent/tests/checkpoint_restore.rs` 底部新增：

```rust
// ─── Group 3: MissingExecutionContext ───────────────────────────

/// G3: 恢复目标节点需要 last_response 但检查点缺失 → MissingExecutionContext。
#[tokio::test]
async fn g3_missing_last_response_rejected() {
    // (a) 直接构造（store=None，跳过最新性）
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        // last_response 默认 None
        let cp = Checkpoint::new(Some(NodeId("tool".into())), &state, hash, 3);
        let config = CheckpointConfig::new(
            |_cp: Checkpoint<AgentState>, _tid: TraceId| {
                Box::pin(async { Ok::<(), CheckpointStoreError>(()) })
            },
            hash,
        );
        let err = agent
            .invoke_with_restore(cp, TraceId::new(), config)
            .await
            .unwrap_err();
        match err {
            LlmError::RestoreFailed { reason, .. } => {
                assert_eq!(reason, RestoreFailureReason::MissingExecutionContext);
            }
            other => panic!("expected RestoreFailed, got {other:?}"),
        }
    }

    // (b) 存储往返（store 存在，最新性通过，last_response 校验失败）
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("x")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<AgentState>::new();
        let trace_id = TraceId::new();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let cp = Checkpoint::new(Some(NodeId("tool".into())), &state, hash, 3);
        let blob = codec.serialize(&cp, hash).unwrap();
        store.save_with_trace(&trace_id, &blob).await.unwrap();
        let loaded = store.load_latest(&trace_id).await.unwrap().unwrap();
        let cp_loaded = codec.deserialize(&loaded, hash).unwrap();
        let config = CheckpointConfig::for_store(store, codec, hash);
        let err = agent
            .invoke_with_restore(cp_loaded, trace_id, config)
            .await
            .unwrap_err();
        match err {
            LlmError::RestoreFailed { reason, .. } => {
                assert_eq!(reason, RestoreFailureReason::MissingExecutionContext);
            }
            other => panic!("expected RestoreFailed, got {other:?}"),
        }
    }

    // (c) 对照：next=budget_check + last_response=None → 正常恢复（不误报）
    {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
        let model = ResolvedModel::new(provider, "test-model");
        let agent = AgentBuilder::new(model).max_iterations(5).compile();
        let hash = agent.graph().canonical_hash();
        let state = AgentState::from_messages(vec![Message::user_text("q")]);
        let cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
        let config = CheckpointConfig::new(
            |_cp: Checkpoint<AgentState>, _tid: TraceId| {
                Box::pin(async { Ok::<(), CheckpointStoreError>(()) })
            },
            hash,
        );
        let result = agent
            .invoke_with_restore(cp, TraceId::new(), config)
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Complete);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p lellm-agent --test checkpoint_restore g3`
Expected: FAIL（`validate_last_response` 仍是桩返回 `Ok`，case a/b 未产生 `MissingExecutionContext`）。

- [ ] **Step 3: 落实 `validate_last_response`**

在 `lellm-agent/src/runtime/checkpoint.rs` 中，把桩：

```rust
pub(crate) fn validate_last_response(
    _cp: &Checkpoint<AgentState>,
) -> Result<(), LlmError> {
    Ok(())
}
```

替换为：

```rust
/// Agent 层校验 — 恢复目标节点对 last_response 的输入完整性（§5.2）。
///
/// `post_llm_check` / `tool` 需要 `last_response = Some`，否则 `MissingExecutionContext`；
/// `budget_check` / `llm` / `compactor` / `end` / `None` 允许 `None`。
pub(crate) fn validate_last_response(
    cp: &Checkpoint<AgentState>,
) -> Result<(), LlmError> {
    match cp.next_node.as_ref().map(|n| n.0.as_str()) {
        Some("post_llm_check") | Some("tool") if cp.state.last_response.is_none() => {
            Err(LlmError::RestoreFailed {
                reason: RestoreFailureReason::MissingExecutionContext,
                message: format!(
                    "node '{}' requires last_response but checkpoint has none (old checkpoint?)",
                    cp.next_node.as_ref().unwrap().0
                ),
            })
        }
        _ => Ok(()),
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p lellm-agent --test checkpoint_restore`
Expected: PASS（G1a/G1b/G3 全绿）。

Run: `cargo test -p lellm-agent`
Expected: PASS（既有 agent 测试全绿，无回归）。

- [ ] **Step 5: 格式化 + 提交**

```bash
cargo fmt
git add lellm-agent/src/runtime/checkpoint.rs lellm-agent/tests/checkpoint_restore.rs
git commit -m "feat(agent): validate_last_response 真实校验 + Group 3 MissingExecutionContext"
```

---

### Task 7: lellm-agent — Group 2（预算延续 + 磁盘往返）+ Group 4（运行期保存失败）

**Files:**
- Test: `lellm-agent/tests/checkpoint_restore.rs`（新增 Group 2 + Group 4）

**Interfaces:**
- Consumes: Task 5 的 `invoke_with_checkpoint` / `invoke_with_restore`、`FileBlobStore`（磁盘往返）、`crash_config`（Task 5 测试基建）、`lellm_graph::BlobCheckpointStore`。
- Produces: 无新 API（纯测试，验证既有行为）。

- [ ] **Step 1: 写 Group 2 + Group 4 测试**

在 `lellm-agent/tests/checkpoint_restore.rs` 底部新增：

```rust
// ─── Group 2: 恢复后再次保存及恢复（预算延续 + 磁盘往返）──────────

/// G2: 恢复后续写同一 trace（FileBlobStore 磁盘往返）；Graph 步数预算 + Agent 业务预算均延续（未重置）。
#[tokio::test]
async fn g2_budget_continues_and_disk_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let store: Arc<dyn lellm_graph::BlobCheckpointStore> =
        Arc::new(FileBlobStore::new(tmp.path().to_path_buf()));
    let codec = SerdeCheckpointCodec::<AgentState>::new();
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("final"),
    ]));
    let model = ResolvedModel::new(provider.clone(), "test-model");
    let agent = AgentBuilder::new(model)
        .max_iterations(5)
        .tools(vec![make_tool(tool_calls.clone())])
        .compile();
    let hash = agent.graph().canonical_hash();
    let trace_id = TraceId::new();

    // 阶段 A：崩溃在 next=budget_check（tool 后）→ latest = next=tool（steps_used=3）
    let crash_store: Arc<InMemoryBlobStore> = Arc::new(InMemoryBlobStore::new());
    let config_a = crash_config(crash_store.clone(), codec.clone(), hash, "budget_check");
    let result_a = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], trace_id, config_a)
        .await;
    assert!(result_a.is_err());

    // 把阶段 A 的检查点搬到磁盘 store（模拟真实持久化路径）
    let blob = crash_store.load_latest(&trace_id).await.unwrap().expect("latest");
    store.save_with_trace(&trace_id, &blob).await.unwrap();
    let cp = codec.deserialize(&blob, hash).unwrap();
    assert_eq!(cp.next_node, Some(NodeId("tool".into())));
    let steps_used_a = cp.steps_used;
    assert_eq!(steps_used_a, 3, "阶段 A 最新检查点 steps_used=3");

    // 阶段 B：从磁盘 store 恢复并续写（同一 trace）
    tool_calls.store(0, Ordering::SeqCst);
    let config_b = CheckpointConfig::for_store(store.clone(), codec.clone(), hash);
    let result_b = agent.invoke_with_restore(cp, trace_id, config_b).await.unwrap();

    // 磁盘往返：最终检查点从磁盘加载
    let final_blob = store.load_latest(&trace_id).await.unwrap().expect("final on disk");
    let final_cp = codec.deserialize(&final_blob, hash).unwrap();
    assert!(
        final_cp.steps_used > steps_used_a,
        "Graph 步数预算延续未重置 ({} > {})",
        final_cp.steps_used,
        steps_used_a
    );
    assert_eq!(final_cp.next_node, None, "完成态");

    // Agent 业务预算：iterations 延续（未重置为 1）
    assert!(result_b.iterations >= 2, "iterations 延续, got: {}", result_b.iterations);
    assert!(
        ContentBlock::flatten_text(&result_b.response.content).contains("final")
    );
    // 磁盘上确实有检查点文件
    assert!(tmp.path().exists());
}

// ─── Group 4: 运行期保存失败接线 ─────────────────────────────────

/// G4: 运行期 checkpoint 保存失败 → 执行在该边界停止，错误映射为 Provider（非 RestoreFailed）。
#[tokio::test]
async fn g4_runtime_save_failure_maps_to_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(),
        text_response("x"),
    ]));
    let model = ResolvedModel::new(provider, "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();

    // save_fn 总是失败（模拟磁盘满）
    let config = CheckpointConfig::new(
        |_cp: Checkpoint<AgentState>, _tid: TraceId| {
            Box::pin(async {
                Err(CheckpointStoreError::Storage("disk full".into()))
            })
        },
        hash,
    );
    let err = agent
        .invoke_with_checkpoint(vec![Message::user_text("q")], TraceId::new(), config)
        .await
        .unwrap_err();
    // 运行期保存失败 → Provider（非 RestoreFailed）
    match err {
        LlmError::Provider { provider, .. } => {
            assert_eq!(provider, "react_graph");
        }
        other => panic!("expected Provider, got {other:?}"),
    }
}

/// G4b: 恢复入口运行期保存失败同样映射为 Provider。
#[tokio::test]
async fn g4b_restore_runtime_save_failure_maps_to_provider() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("ok")]));
    let model = ResolvedModel::new(provider, "test-model");
    let agent = AgentBuilder::new(model).max_iterations(5).compile();
    let hash = agent.graph().canonical_hash();
    let state = AgentState::from_messages(vec![Message::user_text("q")]);
    // 合法检查点（next=budget_check，last_response=None 允许）
    let cp = Checkpoint::new(Some(NodeId("budget_check".into())), &state, hash, 1);
    // save_fn 总是失败
    let config = CheckpointConfig::new(
        |_cp: Checkpoint<AgentState>, _tid: TraceId| {
            Box::pin(async {
                Err(CheckpointStoreError::Storage("disk full".into()))
            })
        },
        hash,
    );
    let err = agent
        .invoke_with_restore(cp, TraceId::new(), config)
        .await
        .unwrap_err();
    match err {
        LlmError::Provider { provider, .. } => {
            assert_eq!(provider, "react_graph");
        }
        other => panic!("expected Provider, got {other:?}"),
    }
}
```

> 说明：G2 用 `InMemoryBlobStore` 模拟阶段 A（崩溃），再把 latest blob 搬到 `FileBlobStore`（磁盘），以验证**磁盘往返**（serialize → 写盘 → 读盘 → deserialize）。阶段 B 的 `config_b` 直接绑定 `FileBlobStore`，恢复后继续写盘，最终从磁盘加载验证预算延续。

- [ ] **Step 2: 运行测试确认通过**

Run: `cargo test -p lellm-agent --test checkpoint_restore`
Expected: PASS（G1a/G1b/G2/G3/G4/G4b 全绿）。

Run: `cargo test -p lellm-agent`
Expected: PASS（既有 agent 测试全绿，无回归）。

- [ ] **Step 3: 运行 workspace 全量测试**

Run: `cargo test`
Expected: PASS（core / graph / agent / provider 全绿，无跨 crate 回归）。

- [ ] **Step 4: 格式化 + 提交**

```bash
cargo fmt
git add lellm-agent/tests/checkpoint_restore.rs
git commit -m "test(agent): Group 2 预算延续+磁盘往返 + Group 4 运行期保存失败映射"
```

---

## Self-Review

**1. Spec 覆盖：**

| Spec 章节 | 对应任务 |
|-----------|----------|
| §2 AgentCheckpoint 三语义 / §3 last_response 边界缺口 | Task 4 |
| §5.2 节点依赖校验表（post_llm_check/tool 需 Some） | Task 6（`validate_last_response`） |
| §5.4 完成态结果构造契约（从 messages 重建） | Task 5（`build_result` + `reconstruct_response_from_messages`） |
| §7.1 两个非流式入口 | Task 5 |
| §7.3 trace_id 绑定规则（首次新鲜 / 恢复最新） | Task 3（`assert_fresh_trace`/`check_restore_latest`）+ Task 5 |
| §8 恢复校验分层（graph 泛型 + Agent 层） | Task 2（graph 泛型）+ Task 6（Agent 层） |
| §9.2 `LlmError::RestoreFailed` + `#[non_exhaustive]` `RestoreFailureReason` | Task 1 |
| §12 四组必测场景 | Task 5（G1）/ Task 6（G3）/ Task 7（G2+G4） |
| §6 不承诺 exactly-once（工具重放接受） | 测试基建注释 + G1a/G1b 断言（`call_count==2`、`tool_calls==1`） |

**2. 占位符扫描：** 无 TBD/TODO/"implement later"。所有代码步骤含完整代码。`validate_last_response` 的 Task 5 桩是**显式标注的临时实现**（Task 6 立即落实），非占位符。

**3. 类型一致性：**
- `invoke_with_checkpoint(messages, trace_id, config)` / `invoke_with_restore(checkpoint, trace_id, config)` — Task 5 定义，Task 6/7 测试调用一致。
- `validate_last_response(&Checkpoint<AgentState>) -> Result<(), LlmError>` — Task 5 桩与 Task 6 真实实现签名一致。
- `map_restore_error` / `map_runtime_error` / `build_result` / `max_steps_for` / `reconstruct_response_from_messages` — Task 5 定义，内部调用一致。
- `RestoreFailureReason` 变体名（Task 1）与 `map_restore_error`（Task 5）/ `validate_last_response`（Task 6）使用一致。
- `CheckpointConfig::check_restore_latest` / `assert_fresh_trace`（Task 3）签名与 Task 5 调用一致。

**4. 已知权衡（执行者须知）：**
- `map_restore_error` 对 `TerminalError::RestoreFailed{reason}` 用字符串匹配区分 `UnsupportedFormat`/`GraphMismatch`（两侧字符串均由本计划控制，稳定）。
- `validate_persistable` 的 `RestoreUnsupported{node,kind}` 经 catch-all 映射为 `RestoreFailed{reason: Other}`（Agent/ReAct 图恒可持久化，此分支实际不触发，仅防御）。
- G2 的 `iterations >= 2` 用宽松断言（避免依赖 LLM 节点 increment 的精确位置）；`steps_used` 用精确值 `3`（已按 run_graph_loop 步序追踪）。
- `ScriptedProvider` 同时实现 `call`/`stream`（Agent LLM 节点走 `stream` 路径；`call` 为 trait 必备）。
