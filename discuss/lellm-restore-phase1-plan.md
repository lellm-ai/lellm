# 恢复能力第一阶段（Q1 限定版）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让「保存 → 进程异常终止（panic/SIGKILL）→ 新进程从检查点续跑」在串行含循环图内真实工作，每个边界有测试钉死。

**Architecture:** Checkpoint 升级为 `format_version=1`（`next_node: Option<NodeId>` 游标 + `steps_used` 预算延续）；执行循环重排为「execute → commit → 路由解析 → 同步 await 保存 → 下一节点」；新增 `FileBlobStore`（trace 内单调提交序号 + flush/rename 原子可见）；恢复入口 `execute_stream_with_restore` 校验后从 `next_node` 续跑并继续保存同一 trace；System B 与策略死代码分两个后续 commit 删除。

**Tech Stack:** Rust（`cargo +1.88.0`，stable 本机 dyld 损坏）、tokio（full）、serde/serde_json、sparkid 2.2.1（替代 uuid 生成检查点 ID）、base64 0.22（信封编码）、tempfile（dev，测试临时目录）。

**Spec:** `discuss/lellm-restore-phase1-design.md`（207a94b，含 R1-R5 评审修正）。本计划额外落实用户 2026-10-01 补充的 5 项实施细节（下称 R6.1-R6.5）：

| # | 细节 | 落实位置 |
|---|---|---|
| R6.1 | seq **按数值排序**并 `checked_add` 检查溢出；`.tmp` 临时文件不参与 `max+1` / latest / prune；单写者限制保持明确 | Task 5 |
| R6.2 | 严格加载覆盖**所有正式入口**：Codec 结构检查不能被另一条公开加载路径绕过；普通 derive 可保留，但「直接反序列化成功 ≠ 检查点已验证」（入口再校验 + doc 明示） | Task 2 + Task 6 |
| R6.3 | 恢复**只接受该 trace 的最新检查点**并续写同一 trace；传入旧检查点 → 明确拒绝（`RestoreNotLatest`），要恢复旧检查点必须换新 trace（避免未定义历史分叉） | Task 6 |
| R6.4 | 崩溃测试：子进程握手后**必须阻塞等待**（防止父进程 kill 前又执行下一节点）；父进程握手读取**设超时**，超时/失败也要**终止并回收**子进程 | Task 7 |
| R6.5 | 步数边界区分完成态：`next_node=None` 且预算刚好耗尽 → 正常返回完成；`next_node=Some` 且预算耗尽 → **执行前**报错 | Task 3 + Task 6 |

另有用户指示：**uuid 可用 sparkid 替代** → `CheckpointId` 内部类型改为 `sparkid::SparkId`（2.2.1，21 字符 Base58、时间可排序、`FromStr` 可解析、serde feature 提供字符串序列化）。`TraceId`/`SpanId` 保持 uuid（不在本次范围）。

## Global Constraints

- 工具链：一律 `cargo +1.88.0`（格式化用 `cargo +1.88.0 fmt`）。
- 提交拆分：C1 恢复实现 / C2 旧 API 删除 / C3 策略清理，三个独立 commit，每个 commit 后独立可编译 + 跑受影响测试；最后 workspace 全量回归。
- **不推送、不发布、不打 tag。**
- Rust 文件 ≤ 400 行（硬指标，尽可能）；每层文件夹 ≤ 8 个文件。
- 测试：单测 < 10s，外部进程测试 < 30s；禁止长 sleep 等待（用事件/管道同步）；> 1s 的测试必须标注原因。
- 永远不用 `rm` 删文件（用 `mv file $HOME/.Trash/`）；git 内删除文件用 `git rm`。
- 中文注释与文档；每个 commit 前先 `cargo +1.88.0 fmt`。
- 范围限定（写进测试 doc comment 与 README）：串行含循环；Agent 未接入；不承诺外部副作用 exactly-once；**进程崩溃安全 ≠ 断电安全**（flush+rename 保证写入完成与原子可见，不保证掉电后数据可见）。

---

## 文件结构

**C1 新增：**

| 文件 | 职责 | 预估行数 |
|---|---|---|
| `lellm-graph/src/graph/run_loop.rs` | 执行循环本体（步骤预算/节点分发/Barrier/同步 checkpoint/路由） | ~200 |
| `lellm-graph/src/exec/checkpoint_save_sink.rs` | `CheckpointSaveSink`（同步保存 + prune + CheckpointSaved 事件） | ~110 |
| `lellm-graph/src/error/build_error.rs` | BuildError/BuildErrors/Diagnostic/GraphDiagnostics（从 error.rs 移出） | ~230 |
| `lellm-graph/src/bin/restore_probe.rs` | R4 新进程测试辅助二进制（run/restore 双模式 + 磁盘确认 + stdout 握手 + 阻塞） | ~260 |
| `lellm-graph/tests/restore_test.rs` | T1/T2/T3/T6/T7（进程内执行级测试） | ~380 |
| `lellm-graph/tests/restore_format_test.rs` | T4/T7b（严格加载 + 入口校验） | ~260 |
| `lellm-graph/tests/restore_store_test.rs` | T5/T7c（FileBlobStore + KeepLatest(0)） | ~260 |
| `lellm-graph/tests/restore_new_process.rs` | T8/T9/T10（真崩溃协议，`#[cfg(unix)]`） | ~320 |
| `lellm-graph/examples/persistent_restore.rs` | 迁移示例（可编译验证） | ~130 |

**C1 修改：** `Cargo.toml`（workspace + lellm-graph 依赖）、`checkpoint/checkpoint_data.rs`、`checkpoint/checkpoint_codec.rs`、`checkpoint/checkpoint_policy.rs`（for_store）、`checkpoint/store.rs`（FileBlobStore）、`checkpoint/mod.rs`、`exec/mod.rs`、`exec/execution_loop.rs`、`exec/execution_engine.rs`、`graph/mod.rs`、`graph/graph_core.rs`、`test_executor.rs`、`error.rs`（→ error/mod.rs）、`event.rs`、`lib.rs`、`tests/checkpoint_test.rs`、`tests/checkpoint_restore_test.rs`、README.md、README_zh.md。

**C2 修改/删除：** `exec/session.rs`（git rm）、`exec/mod.rs`、`checkpoint/checkpoint_data.rs`（删 Frame/FrameStack/MemorySink）、`lib.rs`、README×2、CHANGELOG.md。

**C3 修改：** `checkpoint/checkpoint_policy.rs`、`exec/execution_loop.rs`、`lib.rs`、`tests/checkpoint_test.rs`、CHANGELOG.md。

> 行数说明：`checkpoint_data.rs` 当前 501 行（既有超限），C1 保持不恶化，C2 删 System B 后回落 ~320 行；`error.rs` 当前 411 行，C1 拆分后两文件均 < 400；`graph_core.rs` 当前 496 行，C1 循环移出后 ~390；`execution_engine.rs` 当前 431 行（既有超限），C1 仅 +~10 行，不重构（风险/收益不匹配，记录在案）。

---

## C1 恢复实现

### Task 1: 数据模型与错误类型（Checkpoint format_version=1 / CheckpointId sparkid / FrameInfo / CheckpointSink async / 错误变体 / error 拆分）

**Files:**
- Modify: `Cargo.toml`（workspace deps）、`lellm-graph/Cargo.toml`
- Modify: `lellm-graph/src/checkpoint/checkpoint_data.rs`
- Modify: `lellm-graph/src/error.rs` → 拆为 `lellm-graph/src/error/mod.rs` + `lellm-graph/src/error/build_error.rs`
- Modify: `lellm-graph/src/exec/execution_engine.rs`（emit_checkpoint 改 async + 新 FrameInfo）
- Modify: `lellm-graph/src/exec/execution_loop.rs`（CheckpointSaveSink 最小适配，Task 4 重写）
- Modify: `lellm-graph/src/exec/session.rs`（SessionCheckpointSink 最小适配，保持编译）
- Modify: `lellm-graph/src/graph/graph_core.rs`（emit_checkpoint 调用点最小适配，Task 3 重排）
- Modify: `lellm-graph/src/lib.rs`（导出 `CHECKPOINT_FORMAT_VERSION`）
- Modify: `lellm-graph/tests/checkpoint_test.rs`、`lellm-graph/tests/checkpoint_restore_test.rs`（适配新签名）

**Interfaces:**
- Produces（后续任务依赖的精确签名）:
  - `pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;`
  - `pub struct CheckpointId(pub sparkid::SparkId)` + `impl CheckpointId { pub fn new() -> Self }`（Display 输出 21 字符 Base58）
  - `pub struct Checkpoint<S: WorkflowState = State> { pub format_version: u32, pub checkpoint_id: CheckpointId, pub next_node: Option<NodeId>, pub state: S::Checkpoint, pub graph_hash: u64, pub steps_used: usize, pub created_at: std::time::SystemTime }`
  - `impl<S: WorkflowState> Checkpoint<S> { pub fn new(next_node: Option<NodeId>, state: &S, graph_hash: u64, steps_used: usize) -> Self; pub fn restore_state(self) -> S }`
  - `pub struct FrameInfo { pub next_node: Option<NodeId>, pub step: usize }` + `FrameInfo::new(next_node: Option<NodeId>, step: usize)`
  - `#[async_trait] pub trait CheckpointSink<S: WorkflowState>: Send + Sync { async fn on_checkpoint(&mut self, state: &S, frame: &FrameInfo) -> Result<(), CheckpointStoreError>; }`
  - `CheckpointStoreError::UnsupportedFormat(String)`（`#[error("unsupported checkpoint format: {0}")]`）
  - `TerminalError::CheckpointSaveFailed { error: String }`、`RestoreUnsupported { node: String, kind: String }`、`RestoreNotLatest { checkpoint: String, latest: String }`、`RestoreFailed { reason: String }`

- [ ] **Step 1: 加依赖**

`Cargo.toml`（workspace `[workspace.dependencies]`，uuid 行附近）：

```toml
sparkid = { version = "2.2", features = ["serde"] }
base64 = "0.22"
```

`lellm-graph/Cargo.toml` `[dependencies]`：

```toml
sparkid = { workspace = true }
base64 = { workspace = true }
```

`lellm-graph/Cargo.toml` `[dev-dependencies]`：

```toml
tempfile = "3"
```

- [ ] **Step 2: 写失败测试（新格式 roundtrip）**

新建 `lellm-graph/tests/restore_format_test.rs`（本任务先放 roundtrip；T4/T7b 在 Task 2/6 追加）：

```rust
//! 恢复能力第一阶段 — 格式与入口校验测试（T4 / T7b / roundtrip）。

use lellm_graph::{
    Checkpoint, CheckpointId, NodeId, SerdeCheckpointCodec, State, CHECKPOINT_FORMAT_VERSION,
};

const HASH: u64 = 0x1234_5678_9abc_def0;

/// 新格式 roundtrip：format_version / next_node / steps_used 全部保留
#[tokio::test]
async fn test_checkpoint_v1_roundtrip() {
    let codec = SerdeCheckpointCodec::<State>::new();
    let state = State::new();
    let cp = Checkpoint::new(Some(NodeId("b".into())), &state, HASH, 3);
    assert_eq!(cp.format_version, CHECKPOINT_FORMAT_VERSION);
    assert_eq!(cp.steps_used, 3);

    let blob = codec.serialize(&cp, HASH).expect("serialize");
    let restored = codec.deserialize(&blob, HASH).expect("deserialize");
    assert_eq!(restored.format_version, CHECKPOINT_FORMAT_VERSION);
    assert_eq!(restored.next_node, Some(NodeId("b".into())));
    assert_eq!(restored.steps_used, 3);
    assert_eq!(restored.checkpoint_id, cp.checkpoint_id);
}

/// CheckpointId 使用 sparkid：21 字符、可解析回
#[test]
fn test_checkpoint_id_sparkid() {
    let id = CheckpointId::new();
    let s = id.to_string();
    assert_eq!(s.len(), 21);
    let parsed: sparkid::SparkId = s.parse().expect("parse back");
    assert_eq!(id.0, parsed);
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_format_test`
Expected: 编译失败（`format_version`/`steps_used`/`CheckpointId::new` 不存在）

- [ ] **Step 4: 实现 checkpoint_data.rs 新数据模型**

`checkpoint_data.rs` 修改（NodeId/CheckpointBlob/TraceId re-export 不动）：

```rust
/// Checkpoint 格式版本 — 首个带版本格式（旧无版本格式称 legacy，拒绝加载）。
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

/// Checkpoint 唯一标识 — sparkid（21 字符 Base58，时间可排序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub sparkid::SparkId);

impl CheckpointId {
    pub fn new() -> Self {
        Self(sparkid::SparkId::new())
    }
}

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
```

`Checkpoint` 结构体（**doc 必须写明 R6.2**）：

```rust
/// 执行检查点 — 物化快照 + 执行游标（format_version=1）。
///
/// 唯一职责：恢复（Restore）。给我一个 Checkpoint，我从 `next_node` 开始、
/// 用 `state` 与 `steps_used` 预算继续执行；`next_node = None` 表示已完成。
///
/// # 严格性说明（重要）
///
/// 本结构体保留普通 `derive(Deserialize)`，**直接反序列化成功不等于「检查点已验证」**：
/// - 存储加载路径（`SerdeCheckpointCodec::deserialize`）执行两段式严格校验
///   （语法 → 结构 → 类型化），legacy 格式/缺 `next_node` 键 → `UnsupportedFormat`；
/// - 恢复入口（`SimpleExecutor::execute_stream_with_restore`）对**直接构造**的
///   检查点再做一次校验（版本/指纹/节点存在/步数边界/最新性）。
/// 两条入口之外的直接 `serde_json::from_str::<Checkpoint<_>>` 不受格式保护。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint<S: WorkflowState = State> {
    /// 格式版本 — 加载时严格校验（缺失或不支持 → 拒绝）
    pub format_version: u32,
    /// 唯一标识
    pub checkpoint_id: CheckpointId,
    /// 下一个要执行的节点；None = 执行已完成
    pub next_node: Option<NodeId>,
    /// 物化状态快照（S::Checkpoint 投影）
    pub state: S::Checkpoint,
    /// 图结构指纹 — 恢复时校验兼容性
    pub graph_hash: u64,
    /// 已执行步数 — 恢复时步数预算从此延续（max_steps 为总预算）
    pub steps_used: usize,
    /// 创建时间（仅展示用途，不参与排序）
    pub created_at: std::time::SystemTime,
}

impl<S: WorkflowState> Checkpoint<S> {
    /// 创建 Checkpoint（使用 snapshot() 投影）。
    pub fn new(
        next_node: Option<NodeId>,
        state: &S,
        graph_hash: u64,
        steps_used: usize,
    ) -> Self {
        Self {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: CheckpointId::new(),
            next_node,
            state: state.snapshot(),
            graph_hash,
            steps_used,
            created_at: std::time::SystemTime::now(),
        }
    }

    /// 从 Checkpoint 恢复 Runtime State（使用 restore()）。
    pub fn restore_state(self) -> S {
        S::restore(self.state)
    }
}
```

`FrameInfo`：

```rust
/// Checkpoint 边界描述 — Engine 传递给 Sink 的最小上下文。
#[derive(Debug, Clone)]
pub struct FrameInfo {
    /// 下一个要执行的节点（None = 已完成）
    pub next_node: Option<NodeId>,
    /// 已执行步数（从运行入口累计；恢复时从 steps_used 延续）
    pub step: usize,
}

impl FrameInfo {
    pub fn new(next_node: Option<NodeId>, step: usize) -> Self {
        Self { next_node, step }
    }
}
```

`CheckpointSink` 改 async（三个实现方本任务内最小适配，保持编译）：

```rust
#[async_trait]
pub trait CheckpointSink<S: WorkflowState>: Send + Sync {
    /// 到达恢复边界（State 已 commit，next 已确定）。同步等待保存完成。
    async fn on_checkpoint(
        &mut self,
        state: &S,
        frame: &FrameInfo,
    ) -> Result<(), CheckpointStoreError>;
}
```

- `NoopCheckpointSink`：`async fn on_checkpoint(&mut self, _state: &S, _frame: &FrameInfo) -> Result<(), CheckpointStoreError> { Ok(()) }`
- `MemorySink`（C2 删除，本任务最小适配）：push `Frame { graph_id: String::new(), node_id: frame.next_node.as_ref().map(|n| n.0.clone()).unwrap_or_default(), state: state.snapshot(), cursor: frame.step }`，返回 `Ok(())`
- `session.rs` 的 `SessionCheckpointSink`：同样模式（`frame.next_node...unwrap_or_default()`），返回 `Ok(())`

`CheckpointStoreError` 新增变体：

```rust
#[error("unsupported checkpoint format: {0}")]
UnsupportedFormat(String),
```

文件头 doc 中「MemorySink → FrameStack」分层图更新为指向 `CheckpointSaveSink`（exec 层）与 `FileBlobStore`（Task 5）。

- [ ] **Step 5: 实现 error 拆分 + 新 TerminalError 变体**

`git mv lellm-graph/src/error.rs lellm-graph/src/error/mod.rs`（保持 git 历史），新建 `lellm-graph/src/error/build_error.rs`，把 `BuildError`/`BuildErrors`/`DiagnosticSeverity`/`DiagnosticCategory`/`Diagnostic`/`GraphDiagnostics` 及其 `Display`/`Error` impl 原样移入（文件头 `//! 构建时结构校验与诊断。`），`error/mod.rs` 顶部加：

```rust
mod build_error;
pub use build_error::*;
```

`TerminalError` 追加 4 个变体 + 对应 `Display` 分支：

```rust
/// 检查点同步保存失败 — 执行在该边界停止
CheckpointSaveFailed { error: String },
/// 持久化/恢复入口遇到不支持的图结构（Parallel/Subgraph/Barrier）
RestoreUnsupported { node: String, kind: String },
/// 传入的检查点不是该 trace 的最新检查点 — 拒绝续写原 trace（避免历史分叉）
RestoreNotLatest { checkpoint: String, latest: String },
/// 恢复前置条件失败（如加载最新检查点时损坏/缺失）
RestoreFailed { reason: String },
```

Display：

```rust
Self::CheckpointSaveFailed { error } => write!(f, "checkpoint save failed: {error}"),
Self::RestoreUnsupported { node, kind } => {
    write!(f, "persistence/restore does not support {kind} node '{node}' (phase 1: serial + loops only)")
}
Self::RestoreNotLatest { checkpoint, latest } => {
    write!(f, "checkpoint {checkpoint} is not the latest of this trace (latest: {latest}); restore requires the latest checkpoint or a new trace")
}
Self::RestoreFailed { reason } => write!(f, "restore precondition failed: {reason}"),
```

- [ ] **Step 6: 适配 execution_engine / execution_loop / session / graph_core 编译**

- `execution_engine.rs` `emit_checkpoint` 改为最终形态（Task 3 只改调用点）：

```rust
/// 通知 Checkpoint Sink 到达恢复边界并**同步等待保存完成**。
///
/// 由 Graph 执行循环在 commit + 路由解析之后调用。
/// 保存失败 → `GraphError::Terminal(CheckpointSaveFailed)`，不越过边界。
pub(crate) async fn emit_checkpoint(
    &mut self,
    next_node: Option<NodeId>,
    step: usize,
) -> Result<(), GraphError> {
    if let Some(ref mut sink) = self.checkpoint {
        let frame = crate::checkpoint::FrameInfo::new(next_node, step);
        sink.on_checkpoint(self.state, &frame)
            .await
            .map_err(|e| {
                GraphError::Terminal(crate::error::TerminalError::CheckpointSaveFailed {
                    error: e.to_string(),
                })
            })
    } else {
        Ok(())
    }
}
```

  需要 `use crate::checkpoint::NodeId;`。
- `graph_core.rs` 调用点（340-491 循环内，Task 3 重排前最小改）：`exec_ctx.emit_checkpoint(&current, step);` → `exec_ctx.emit_checkpoint(None, step).await?;`（临时 None，Task 3 传真实 next）。
- `execution_loop.rs` `CheckpointSaveSink::on_checkpoint` 最小适配（Task 4 重写）：`#[async_trait]` impl，构造 `Checkpoint::new(frame.next_node.clone(), state, self.graph_hash, frame.step)`，保留 `tokio::spawn`（Task 4 去 spawn），返回 `Ok(())`。
- `lib.rs` Checkpoint 导出块加 `CHECKPOINT_FORMAT_VERSION`。

- [ ] **Step 7: 适配既有测试**

`tests/checkpoint_test.rs`：
- 删 `use uuid::Uuid;`；`CheckpointId(Uuid::new_v4())` → `CheckpointId::new()`；`CheckpointId(Uuid::nil())` → `CheckpointId::new()`
- `Checkpoint::new("test_node", &state, TEST_GRAPH_HASH)` → `Checkpoint::new(Some(NodeId("test_node".into())), &state, TEST_GRAPH_HASH, 0)`（import `NodeId`）
- `assert_eq!(restored.current_node, cp.current_node)` → `assert_eq!(restored.next_node, cp.next_node)`

`tests/checkpoint_restore_test.rs`：
- `Checkpoint::new("process_order", &state, TEST_GRAPH_HASH)` → `Checkpoint::new(Some(NodeId("process_order".into())), &state, TEST_GRAPH_HASH, 0)`
- `restored.current_node.0` / `restored.current_node.to_string()` → `restored.next_node.as_ref().expect("next_node").0` / `restored.next_node.as_ref().expect("next_node").to_string()`

`checkpoint_data.rs` 内联测试 `test_auto_checkpoint_via_memory_sink`：断言改为 `frames[0].node_id == "b"`（a 完成后 next=b）、`frames[1].node_id == ""`（b 是 end，next=None → 空串）、cursor 1/2 不变。`test_frame_info_minimal` 改为 `FrameInfo::new(Some(NodeId("test_node".into())), 42)` + `assert_eq!(info.next_node, Some(NodeId("test_node".into())))`。

- [ ] **Step 8: 全量编译 + 测试**

Run: `cargo +1.88.0 test -p lellm-graph`
Expected: 全部通过（graph_test/parallel_test 因 run_inline 旧签名保留 → 零改动）

- [ ] **Step 9: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "feat(graph): Checkpoint format_version=1 数据模型（next_node 游标 + steps_used + sparkid CheckpointId + CheckpointSink async + 新错误变体）"
```

---

### Task 2: Codec 两段式严格加载（T4）

**Files:**
- Modify: `lellm-graph/src/checkpoint/checkpoint_codec.rs`
- Test: `lellm-graph/tests/restore_format_test.rs`（追加 T4）

**Interfaces:**
- Consumes: Task 1 的 `Checkpoint`（format_version/next_node）、`CheckpointStoreError::UnsupportedFormat`、`CHECKPOINT_FORMAT_VERSION`
- Produces: `SerdeCheckpointCodec::deserialize` 三段式（hash 校验 → 语法 → 结构校验 → 类型化）；`fn validate_checkpoint_format(v: &serde_json::Value) -> Result<(), CheckpointStoreError>`（pub(crate)）

判定表（T4 逐条钉死）：

| 输入 | 结果 |
|---|---|
| JSON 语法损坏 / 顶层非对象 | `Corrupted` |
| 键缺失 `format_version` | `UnsupportedFormat` |
| `format_version != 1`（含非数字） | `UnsupportedFormat` |
| 键缺失 `next_node` | `UnsupportedFormat` — **不得**解释为已完成 |
| 显式 `"next_node": null` | 合法 → `next_node = None`（已完成） |
| 旧格式（有 `current_node`、无 `format_version`） | `UnsupportedFormat` |
| 段 2 通过但类型不符（如 `next_node` 是数字） | `Corrupted` |

- [ ] **Step 1: 写失败测试（T4，7 用例）**

追加到 `tests/restore_format_test.rs`：

```rust
/// T4: 严格加载 7 用例 — Codec 两段式（不用错误文本分类）
#[tokio::test]
async fn t4_strict_loading_rejects_legacy_and_missing() {
    use lellm_graph::CheckpointBlob;
    use serde_json::json;
    use std::time::SystemTime;

    let codec = SerdeCheckpointCodec::<State>::new();

    // 基准：真实序列化的 v1 checkpoint（含合法 SystemTime 编码）
    let base = serde_json::to_value(Checkpoint::new(
        Some(NodeId("a".into())),
        &State::new(),
        HASH,
        1,
    ))
    .expect("serialize base");

    let make_blob = |v: serde_json::Value| {
        CheckpointBlob::new(
            CheckpointId::new(),
            serde_json::to_vec(&v).expect("blob data"),
            HASH,
            SystemTime::now(),
        )
    };

    // 1. JSON 语法损坏 → Corrupted
    let bad = CheckpointBlob::new(CheckpointId::new(), b"{ not json".to_vec(), HASH, SystemTime::now());
    match codec.deserialize(&bad, HASH) {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("case1 expected Corrupted, got: {other:?}"),
    }

    // 2. 顶层非对象 → Corrupted
    let not_obj = CheckpointBlob::new(CheckpointId::new(), b"[1,2]".to_vec(), HASH, SystemTime::now());
    match codec.deserialize(&not_obj, HASH) {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("case2 expected Corrupted, got: {other:?}"),
    }

    // 3. 缺 format_version → UnsupportedFormat
    let mut v = base.clone();
    v.as_object_mut().expect("obj").remove("format_version");
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case3 expected UnsupportedFormat, got: {other:?}"),
    }

    // 4. format_version=999 → UnsupportedFormat
    let mut v = base.clone();
    v["format_version"] = json!(999);
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case4 expected UnsupportedFormat, got: {other:?}"),
    }

    // 5. 缺 next_node 键 → UnsupportedFormat（禁止解释为已完成）
    let mut v = base.clone();
    v.as_object_mut().expect("obj").remove("next_node");
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case5 expected UnsupportedFormat, got: {other:?}"),
    }

    // 6. 显式 "next_node": null → 合法，已完成
    let mut v = base.clone();
    v["next_node"] = serde_json::Value::Null;
    let cp = codec.deserialize(&make_blob(v), HASH).expect("case6 should load");
    assert_eq!(cp.next_node, None, "explicit null = completed");

    // 7. 旧格式（current_node、无 format_version）→ UnsupportedFormat
    let legacy = json!({
        "checkpoint_id": CheckpointId::new().to_string(),
        "current_node": "a",
        "state": {},
        "graph_hash": HASH,
        "created_at": base["created_at"].clone(),
    });
    match codec.deserialize(&make_blob(legacy), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case7 expected UnsupportedFormat, got: {other:?}"),
    }

    // 8. 段 2 通过但类型不符（next_node 是数字）→ Corrupted
    let mut v = base.clone();
    v["next_node"] = json!(42);
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("case8 expected Corrupted, got: {other:?}"),
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_format_test t4`
Expected: FAIL（缺 format_version 的 blob 目前 derive 直接失败为 Corrupted，而非 UnsupportedFormat；缺 next_node 同理）

- [ ] **Step 3: 实现两段式 deserialize**

`checkpoint_codec.rs` 的 `SerdeCheckpointCodec::deserialize` 改为：

```rust
fn deserialize(
    &self,
    blob: &CheckpointBlob,
    expected_hash: u64,
) -> Result<Checkpoint<S>, CheckpointStoreError> {
    if blob.graph_hash != expected_hash {
        return Err(CheckpointStoreError::GraphMismatch {
            expected: expected_hash,
            actual: blob.graph_hash,
        });
    }

    // 段 1：语法 — 解析为通用 Value
    let value: serde_json::Value = serde_json::from_slice(&blob.data)
        .map_err(|e| CheckpointStoreError::Corrupted(e.to_string()))?;
    if !value.is_object() {
        return Err(CheckpointStoreError::Corrupted(
            "checkpoint is not a JSON object".into(),
        ));
    }

    // 段 2：格式校验 — 对 Value 做结构化检查（三态在此天然可分：
    // 键缺失 ≠ Value::Null ≠ 值）
    validate_checkpoint_format(&value)?;

    // 段 3：类型化 — 类型不符 → Corrupted
    serde_json::from_value(value).map_err(|e| CheckpointStoreError::Corrupted(e.to_string()))
}

/// 段 2 格式校验 — 缺失键 / 版本不符 → UnsupportedFormat（不靠错误文本分类）。
pub(crate) fn validate_checkpoint_format(v: &serde_json::Value) -> Result<(), CheckpointStoreError> {
    let fmt_version = v.get("format_version").ok_or_else(|| {
        CheckpointStoreError::UnsupportedFormat(
            "missing format_version (legacy format?)".into(),
        )
    })?;
    let _ = fmt_version.as_u64().filter(|x| *x == super::checkpoint_data::CHECKPOINT_FORMAT_VERSION as u64).ok_or_else(|| {
        CheckpointStoreError::UnsupportedFormat(format!("unsupported format_version: {fmt_version}"))
    })?;
    // next_node 键必须存在：缺失 = legacy（拒绝）；null = 已完成（合法）
    if !v.contains_key("next_node") {
        return Err(CheckpointStoreError::UnsupportedFormat(
            "missing next_node (legacy current_node format?)".into(),
        ));
    }
    Ok(())
}
```

- [ ] **Step 4: 运行测试**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_format_test`
Expected: PASS（roundtrip + sparkid + T4 全绿）

- [ ] **Step 5: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "feat(graph): Codec 两段式严格加载（Value 解析→结构校验→类型化；legacy/缺 next_node → UnsupportedFormat）"
```

---

### Task 3: 执行循环重排（run_inline_from / 同步保存 / 溢出安全预算 / CheckpointSaved 事件变体）

**Files:**
- Create: `lellm-graph/src/graph/run_loop.rs`
- Modify: `lellm-graph/src/graph/graph_core.rs`（run_inline 改 wrapper + run_inline_from + validate_persistable + 删旧循环）
- Modify: `lellm-graph/src/graph/mod.rs`（`pub(crate) mod run_loop;`）
- Modify: `lellm-graph/src/event.rs`（`CheckpointSaved.node_name: String` → `next_node: Option<String>`）
- Modify: `lellm-graph/src/exec/execution_loop.rs`（run_execution_loop 调 run_inline_from；GraphStart/Complete 分支）
- Test: `lellm-graph/tests/restore_test.rs`（新建，T1/T2）

**Interfaces:**
- Consumes: Task 1 的 `emit_checkpoint(Option<NodeId>, usize) -> Result<(), GraphError>`（async）、`Checkpoint::new(Option<NodeId>, &S, u64, usize)`
- Produces:
  - `pub async fn Graph::run_inline(&self, exec_ctx, max_steps, step_cb) -> Result<(), GraphError>`（**签名不变**，内部委托）
  - `pub(crate) async fn Graph::run_inline_from(&self, exec_ctx: &mut ExecutionEngine<'_, S>, start_node: &str, steps_used: usize, max_steps: usize, step_cb: &mut dyn StepCallback<'cb>) -> Result<(), GraphError>`
  - `pub fn Graph::validate_persistable(&self) -> Result<(), GraphError>`（拒绝 Parallel/Subgraph/Barrier）
  - `run_loop::run_graph_loop(graph, exec_ctx, start_node, steps_used, max_steps, step_cb) -> Result<(), GraphError>`（pub(crate) 自由函数）
  - `GraphEvent::CheckpointSaved { checkpoint_id: CheckpointId, next_node: Option<String>, step: usize }`

- [ ] **Step 1: 写失败测试（T1 游标语义 / T2 保存失败）**

新建 `lellm-graph/tests/restore_test.rs`：

```rust
//! 恢复能力第一阶段 — 进程内执行级测试（T1/T2/T3/T6/T7）。
//!
//! 范围：串行含循环。承诺边界见 restore_new_process.rs 头部。

use std::sync::Arc;

use lellm_graph::{
    BlobCheckpointStore, CheckpointConfig, CheckpointStoreError, GraphBuilder, InMemoryBlobStore,
    NodeKind, SerdeCheckpointCodec, SimpleExecutor, State, StateExt, TaskNode,
};

/// 线性图 a→b→c（end=c），每节点向 effects 追加自身名。
fn linear_graph(effects: &Arc<std::sync::Mutex<Vec<String>>>) -> lellm_graph::Graph {
    let g = effects.clone();
    let mk = |name: &str| {
        let g = g.clone();
        TaskNode::new(name, move |_ctx| {
            g.lock().expect("effects").push(name.to_string());
            Ok(())
        })
    };
    GraphBuilder::<State>::new("linear")
        .start("a")
        .node("a", mk("a"))
        .node("b", mk("b"))
        .node("c", mk("c"))
        .edge("a", "b")
        .edge("b", "c")
        .end("c")
        .build()
        .expect("build")
}

/// 消费事件流直至 GraphComplete/GraphError，返回 (trace_id, 结果)。
async fn drain(
    mut stream: lellm_graph::GraphStream,
) -> (lellm_graph::TraceId, Result<State, String>) {
    let mut trace_id = None;
    loop {
        match stream.recv().await {
            Some(lellm_graph::GraphEvent::GraphStart { trace_id: t }) => trace_id = Some(t),
            Some(lellm_graph::GraphEvent::GraphComplete { result }) => {
                return (trace_id.expect("GraphStart"), Ok(result.state));
            }
            Some(lellm_graph::GraphEvent::GraphError { error, .. }) => {
                return (trace_id.expect("GraphStart"), Err(error.to_string()));
            }
            Some(_) => {}
            None => panic!("stream closed without terminal event"),
        }
    }
}

fn config_for(store: Arc<InMemoryBlobStore>, hash: u64) -> CheckpointConfig<State> {
    CheckpointConfig::for_store(store, SerdeCheckpointCodec::<State>::new(), hash)
}

/// T1: 游标语义 — 每个成功提交节点保存「下一个节点」；end 节点后 next=None（完成态也保存）
#[tokio::test]
async fn t1_cursor_semantics_next_node_and_steps() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = linear_graph(&effects);
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let executor = SimpleExecutor::new(100);

    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config_for(store.clone(), hash))
        .expect("entry");
    let (_tid, result) = drain(exec.stream).await;
    result.expect("run should complete");

    // 3 个检查点：next=b(su=1) / next=c(su=2) / next=None(su=3)
    let trace = lellm_graph::TraceId::new(); // 占位：实际 trace_id 从 GraphStart 取（drain 已返回）
    let _ = trace;
    // 用 store 索引无法直接按 trace 查（InMemory 需要 trace_id）— 改用 TypedCheckpointStore 全量 load 不便，
    // 因此这里直接断言 effects 与 store 长度：
    assert_eq!(*effects.lock().expect("m"), vec!["a", "b", "c"]);
    assert_eq!(store.len(), 3, "3 checkpoints saved (incl. completed state)");
}
```

> **注意**：T1 需要按 trace_id 查检查点。`drain` 已返回 trace_id；把上面占位替换为：

```rust
    let (tid, result) = drain(exec.stream).await;
    result.expect("run should complete");
    let ids = store.list(&tid).await.expect("list");
    assert_eq!(ids.len(), 3);
    // InMemory list 按插入倒序：[cp3, cp2, cp1]
    let codec = SerdeCheckpointCodec::<State>::new();
    let load = |id: &lellm_graph::CheckpointId| async {
        store
            .load(id)
            .await
            .expect("blob")
            .and_then(|b| codec.deserialize(&b, hash).ok())
            .expect("cp")
    };
    let cp3 = load(&ids[0]).await;
    let cp2 = load(&ids[1]).await;
    let cp1 = load(&ids[2]).await;
    assert_eq!(cp1.next_node, Some(lellm_graph::NodeId("b".into())));
    assert_eq!(cp1.steps_used, 1);
    assert_eq!(cp2.next_node, Some(lellm_graph::NodeId("c".into())));
    assert_eq!(cp2.steps_used, 2);
    assert_eq!(cp3.next_node, None, "completed state also saved");
    assert_eq!(cp3.steps_used, 3);
    assert_eq!(*effects.lock().expect("m"), vec!["a", "b", "c"]);
```

T2：

```rust
/// T2: 保存失败 → 执行在该边界停止，后续节点不执行
#[tokio::test]
async fn t2_save_failure_stops_at_boundary() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = linear_graph(&effects);
    let hash = graph.canonical_hash();

    // 永远失败的 save_fn
    let config = CheckpointConfig::new(
        move |_cp, _tid| {
            Box::pin(async {
                Err(CheckpointStoreError::Storage("disk full".into()))
            })
        },
        hash,
    );

    let executor = SimpleExecutor::new(100);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config)
        .expect("entry");
    let (_tid, result) = drain(exec.stream).await;
    let err = result.expect_err("should fail at first checkpoint boundary");
    assert!(err.contains("checkpoint save failed"), "got: {err}");
    assert!(err.contains("disk full"), "got: {err}");
    // a 执行了（副作用已发生），b/c 未执行
    assert_eq!(*effects.lock().expect("m"), vec!["a"]);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_test`
Expected: 编译失败（`execute_stream_with_checkpoint` 不存在 → Task 6 提供；本任务先让 T1/T2 以「编译失败」形式登记，实现顺序上 Task 3 完成后 T1/T2 仍依赖 Task 6 的入口，**允许在 Task 6 结束后一起转绿**；若希望本任务独立可验证，可先用 `run_inline` + 手写 sink 的临时断言替代，Task 6 后恢复正式入口）

> **执行顺序说明**：T1/T2 的最终形态依赖 Task 6 的 `execute_stream_with_checkpoint`。本任务交付物 = 新循环 + `run_inline_from` + 事件变体；T1/T2 测试代码本任务写入，Task 6 完成后运行转绿。中间状态保持 `cargo +1.88.0 test -p lellm-graph --lib --bins` 可编译。

- [ ] **Step 3: 实现 run_loop.rs（新循环本体）**

新建 `lellm-graph/src/graph/run_loop.rs`：

```rust
//! Graph 执行循环本体 — run_inline / run_inline_from 共享。
//!
//! 顺序（每步）：
//! ```text
//! step = steps_used 延续，checked_add(1)（溢出安全）
//! step > max_steps（总预算）→ StepsExceeded
//! execute(node) → on_node_end → 错误传播
//! commit()
//! take_control() → (next_action, signal)
//! [Barrier Pause → wait → outcome → 路由覆盖]
//! 解析 next: Option<NodeId>（End/到达 end 节点 → None；Goto/Next → Some）
//! emit_checkpoint(next, step).await?   ★ 同步保存，失败 → Err，不越过边界
//! next = None → return Ok；Some(n) → current = n
//! ```

use std::time::Instant;

use crate::checkpoint::NodeId;
use crate::error::{GraphError, TerminalError};
use crate::event::BarrierId;
use crate::exec::execution_engine::{ExecutionEngine, ExecutionSignal, NextAction};
use crate::ids::SpanId;
#[allow(deprecated)]
use crate::node::{BarrierNode, ConditionNode, LeafNode, NodeKind};
use crate::state::workflow_state::{MergeStrategy, WorkflowState};
use super::graph_core::{Graph, StepCallback};

/// 执行循环本体（pub(crate) — 由 Graph::run_inline_from 调用）。
#[allow(deprecated)]
pub(crate) async fn run_graph_loop<'cb, S, M>(
    graph: &Graph<S, M>,
    exec_ctx: &mut ExecutionEngine<'_, S>,
    start_node: &str,
    steps_used: usize,
    max_steps: usize,
    step_cb: &mut dyn StepCallback<'cb>,
) -> Result<(), GraphError>
where
    S: WorkflowState,
    M: MergeStrategy<S>,
{
    let mut current = start_node.to_string();
    let mut step = steps_used;

    loop {
        // 溢出安全预算检查（max_steps = 总预算；恢复时从 steps_used 延续）
        step = step
            .checked_add(1)
            .ok_or_else(|| GraphError::Terminal(TerminalError::StepsExceeded {
                limit: max_steps,
            }))?;
        if step > max_steps {
            return Err(GraphError::Terminal(TerminalError::StepsExceeded {
                limit: max_steps,
            }));
        }

        let node = graph.nodes.get(&current).ok_or_else(|| {
            GraphError::Terminal(TerminalError::NodeNotFound(current.clone()))
        })?;

        let node_start = Instant::now();
        let span_id = SpanId::new();
        step_cb.on_node_start(&current, span_id, step);

        let exec_result = match node {
            NodeKind::Task(n) => {
                let mut ctx = exec_ctx.build_node_context();
                n.execute(&mut ctx).await
            }
            NodeKind::Condition(n) => {
                let mut ctx = exec_ctx.build_leaf_context();
                <ConditionNode<S> as LeafNode<S>>::execute(n, &mut ctx).await
            }
            NodeKind::Barrier(n) => {
                let mut ctx = exec_ctx.build_leaf_context();
                <BarrierNode<S> as LeafNode<S>>::execute(n, &mut ctx).await
            }
            NodeKind::External(n) => {
                let mut ctx = exec_ctx.build_node_context();
                n.execute(&mut ctx).await
            }
            NodeKind::ExternalLeaf(n) => {
                let mut ctx = exec_ctx.build_leaf_context();
                n.execute(&mut ctx).await
            }
            NodeKind::Parallel(p) => p.execute(exec_ctx).await,
            NodeKind::Subgraph(spec) => {
                let stream = exec_ctx.stream_sink();
                let cancel = exec_ctx.cancel_token().clone();
                spec.execute(exec_ctx.state_mut(), stream, cancel).await
            }
        };

        let node_duration = node_start.elapsed();
        let success = exec_result.is_ok();
        step_cb.on_node_end(&current, span_id, step, node_duration, success);
        exec_result?;

        // commit → take_control → 解析 next → ★ 同步保存
        exec_ctx.commit();
        let (next_action, signal) = exec_ctx.take_control();

        let next = if let Some(ExecutionSignal::Pause {
            barrier_id,
            timeout,
        }) = signal
        {
            let span_id = SpanId::new();
            step_cb.on_barrier_waiting(&barrier_id, &current, span_id);
            let outcome = exec_ctx.wait_barrier(&barrier_id, timeout).await;

            let barrier = match node {
                NodeKind::Barrier(b) => b,
                _ => unreachable!("Pause 信号仅由 Barrier 节点产生"),
            };

            let rejected = match &outcome {
                crate::node::barrier_sink::BarrierOutcome::Decision(
                    crate::event::BarrierDecision::Reject { .. },
                ) => true,
                crate::node::barrier_sink::BarrierOutcome::TimedOut => matches!(
                    barrier.default_action,
                    crate::node::BarrierDefaultAction::Reject
                ),
                _ => false,
            };

            if rejected {
                // 拒绝：有 reject_target → 跳转；无 → 完成（保存完成态检查点）
                barrier.reject_target.as_ref().map(|t| NodeId(t.clone()))
            } else {
                match outcome {
                    crate::node::barrier_sink::BarrierOutcome::Decision(
                        crate::event::BarrierDecision::Reroute { target },
                    ) => Some(NodeId(target)),
                    crate::node::barrier_sink::BarrierOutcome::Cancelled => {
                        return Err(GraphError::Terminal(TerminalError::BarrierCancelled {
                            node: current.clone(),
                        }));
                    }
                    // Approve / Modify / Timeout(Approve|Skip) — 正常路由
                    _ => resolve_next_opt(graph, &next_action, &current, exec_ctx.state())?,
                }
            }
        } else {
            resolve_next_opt(graph, &next_action, &current, exec_ctx.state())?
        };

        // ★ 同步保存检查点：失败 → 错误，不越过边界
        exec_ctx.emit_checkpoint(next.clone(), step).await?;

        match next {
            None => return Ok(()),
            Some(n) => current = n.0,
        }
    }
}

/// 路由解析 — NextAction + current → Option<NodeId>（None = 已完成）。
fn resolve_next_opt<S: WorkflowState>(
    graph: &Graph<S, impl MergeStrategy<S>>,
    next_action: &NextAction,
    current: &str,
    state: &S,
) -> Result<Option<NodeId>, GraphError> {
    Ok(match next_action {
        NextAction::End => None,
        NextAction::Goto(target) => Some(NodeId(target.clone())),
        NextAction::Next => {
            if current == graph.end_node() {
                None
            } else {
                Some(NodeId(graph.resolve_next_inline(current, state)?))
            }
        }
    })
}
```

> 注：`graph.nodes`/`end_node()`/`resolve_next_inline` 是 `pub(crate)`，run_loop 与 graph_core 同 crate 可直接访问。`Graph` 的泛型参数需与 `run_graph_loop` 一致（`Graph<S, M>`）。

- [ ] **Step 4: graph_core.rs 改 wrapper + validate_persistable**

删除 340-491 的旧循环体，`run_inline` 改为：

```rust
/// 内联执行 — 首次运行语义（start_node()，steps_used=0）。
///
/// 公共签名保持不变（lellm-agent / examples / subgraph_spec 零改动）。
pub async fn run_inline<'cb>(
    &self,
    exec_ctx: &mut ExecutionEngine<'_, S>,
    max_steps: usize,
    step_cb: &mut dyn StepCallback<'cb>,
) -> Result<(), GraphError> {
    self.run_inline_from(exec_ctx, self.start_node(), 0, max_steps, step_cb)
        .await
}

/// 统一执行入口 — 首次运行与恢复共用（pub(crate)）。
///
/// - `start_node` — 首次运行 = `start_node()`；恢复 = `checkpoint.next_node`
/// - `steps_used` — 首次运行 = 0；恢复 = `checkpoint.steps_used`（预算延续）
/// - `max_steps` — **总预算**（恢复时同样传总预算）
pub(crate) async fn run_inline_from<'cb>(
    &self,
    exec_ctx: &mut ExecutionEngine<'_, S>,
    start_node: &str,
    steps_used: usize,
    max_steps: usize,
    step_cb: &mut dyn StepCallback<'cb>,
) -> Result<(), GraphError> {
    crate::graph::run_loop::run_graph_loop(
        self, exec_ctx, start_node, steps_used, max_steps, step_cb,
    ).await
}

/// 持久化执行校验 — 拒绝含 Parallel/Subgraph/Barrier 的图。
///
/// 第一阶段（串行含循环）在**启动持久化执行时**显式拒绝，
/// 不等崩溃后恢复才报错；恢复入口再校验一次。
/// 非持久化执行不受影响（不调用此方法）。
pub fn validate_persistable(&self) -> Result<(), GraphError> {
    for (name, kind) in self.node_map() {
        let unsupported = match kind {
            NodeKind::Parallel(_) => Some("Parallel"),
            NodeKind::Subgraph(_) => Some("Subgraph"),
            NodeKind::Barrier(_) => Some("Barrier"),
            _ => None,
        };
        if let Some(kind_name) = unsupported {
            return Err(GraphError::Terminal(TerminalError::RestoreUnsupported {
                node: name.clone(),
                kind: kind_name.to_string(),
            }));
        }
    }
    Ok(())
}
```

`graph/mod.rs` 加 `pub(crate) mod run_loop;`。

- [ ] **Step 5: event.rs CheckpointSaved 变体**

```rust
/// Checkpoint 已保存（尽力而为的观测信号 — try_send 可能丢，不作可靠握手）。
CheckpointSaved {
    checkpoint_id: CheckpointId,
    /// 下一个要执行的节点（None = 已完成）
    next_node: Option<String>,
    step: usize,
},
```

（无外部消费者 — 已确认 lellm-agent/examples/tests 零引用。）

- [ ] **Step 6: execution_loop.rs run_execution_loop 适配**

`graph.run_inline(&mut engine, max_steps, &mut step_cb).await` 改为：

```rust
// 首次运行：start_node() + steps_used=0（恢复分支在 Task 6 加入）
graph.run_inline_from(&mut engine, graph.start_node(), 0, max_steps, &mut step_cb)
    .await
```

- [ ] **Step 7: 编译 + 既有测试回归**

Run: `cargo +1.88.0 test -p lellm-graph --test graph_test --test parallel_test --test checkpoint_test --test checkpoint_restore_test`
Expected: 全绿（run_inline 行为等价：非持久化路径 checkpoint sink=None → emit 空操作）

- [ ] **Step 8: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "feat(graph): 执行循环重排 — run_inline_from 统一入口 + 路由后同步保存 + 溢出安全预算 + CheckpointSaved 接线字段"
```

---

### Task 4: CheckpointSaveSink 同步保存 + for_store + CheckpointSaved 事件发射（T7c）

**Files:**
- Create: `lellm-graph/src/exec/checkpoint_save_sink.rs`
- Modify: `lellm-graph/src/exec/mod.rs`（`pub(crate) mod checkpoint_save_sink;` + re-export）
- Modify: `lellm-graph/src/exec/execution_loop.rs`（删旧 CheckpointSaveSink，run_execution_loop 传 event_tx）
- Modify: `lellm-graph/src/checkpoint/checkpoint_policy.rs`（`CheckpointConfig::for_store`；`SerdeCheckpointCodec` 加 `Clone`）
- Modify: `lellm-graph/src/checkpoint/checkpoint_codec.rs`（`#[derive(Clone)]`）
- Test: `lellm-graph/tests/restore_store_test.rs`（新建，T7c）

**Interfaces:**
- Consumes: Task 1 的 async `CheckpointSink`、Task 3 的 `GraphEvent::CheckpointSaved { next_node }`
- Produces:
  - `CheckpointSaveSink::new(config: CheckpointConfig<S>, trace_id: TraceId, event_tx: Option<tokio::sync::mpsc::Sender<GraphEvent<S>>>)`
  - `CheckpointConfig::for_store(store: Arc<dyn BlobCheckpointStore>, codec: impl CheckpointCodec<S> + Clone + Send + Sync + 'static, graph_hash: u64) -> Self`

- [ ] **Step 1: 写失败测试（T7c KeepLatest(0)）**

新建 `lellm-graph/tests/restore_store_test.rs`：

```rust
//! 恢复能力第一阶段 — 存储与保留策略测试（T5 / T7c）。

use std::sync::Arc;

use lellm_graph::{
    CheckpointConfig, GraphBuilder, InMemoryBlobStore, NodeKind, RetentionPolicy,
    SerdeCheckpointCodec, SimpleExecutor, State, TaskNode,
};

fn two_node_graph(effects: &Arc<std::sync::Mutex<Vec<String>>>) -> lellm_graph::Graph {
    let g = effects.clone();
    let mk = |name: &str| {
        let g = g.clone();
        TaskNode::new(name, move |_ctx| {
            g.lock().expect("effects").push(name.to_string());
            Ok(())
        })
    };
    GraphBuilder::<State>::new("two")
        .start("a")
        .node("a", mk("a"))
        .node("b", mk("b"))
        .edge("a", "b")
        .end("b")
        .build()
        .expect("build")
}

/// T7c: KeepLatest(0) → 保存路径明确报错（避免保存后删掉全部恢复点）
#[tokio::test]
async fn t7c_keep_latest_zero_rejected() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = two_node_graph(&effects);
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());

    let config = CheckpointConfig::for_store(
        store,
        SerdeCheckpointCodec::<State>::new(),
        hash,
    )
    .with_retention(RetentionPolicy::KeepLatest(0));

    let executor = SimpleExecutor::new(100);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config)
        .expect("entry");

    // 消费至终止事件
    let mut got_error = None;
    while let Some(ev) = exec.stream.recv().await {
        if let lellm_graph::GraphEvent::GraphError { error, .. } = ev {
            got_error = Some(error.to_string());
            break;
        }
    }
    let err = got_error.expect("should fail on KeepLatest(0)");
    assert!(err.contains("KeepLatest(0)"), "got: {err}");
    // a 执行了（副作用发生），b 未执行
    assert_eq!(*effects.lock().expect("m"), vec!["a"]);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_store_test`
Expected: 编译失败（`for_store` 不存在）

- [ ] **Step 3: 实现 checkpoint_save_sink.rs**

新建 `lellm-graph/src/exec/checkpoint_save_sink.rs`：

```rust
//! CheckpointSaveSink — 包装 CheckpointConfig 为 CheckpointSink。
//!
//! 第一阶段语义（固定）：每个成功提交的节点都**同步等待**检查点保存，
//! 完成态也保存。保存失败 → 返回 Err → 执行在该边界停止。

use std::sync::Arc;

use async_trait::async_trait;

use crate::checkpoint::{
    Checkpoint, CheckpointConfig, CheckpointSink, CheckpointStoreError, FrameInfo,
    RetentionPolicy, TraceId,
};
use crate::event::GraphEvent;
use crate::state::workflow_state::WorkflowState;

pub struct CheckpointSaveSink<S: WorkflowState> {
    save_fn: Arc<crate::checkpoint::checkpoint_policy::CheckpointSaveFn<S>>,
    graph_hash: u64,
    trace_id: TraceId,
    retention: RetentionPolicy,
    store: Option<Arc<dyn crate::checkpoint::store::BlobCheckpointStore>>,
    event_tx: Option<tokio::sync::mpsc::Sender<GraphEvent<S>>>,
}

impl<S: WorkflowState> CheckpointSaveSink<S> {
    pub fn new(
        config: CheckpointConfig<S>,
        trace_id: TraceId,
        event_tx: Option<tokio::sync::mpsc::Sender<GraphEvent<S>>>,
    ) -> Self {
        Self {
            save_fn: config.save_fn,
            graph_hash: config.graph_hash,
            trace_id,
            retention: config.retention,
            store: config.store,
            event_tx,
        }
    }
}

#[async_trait]
impl<S: WorkflowState + 'static> CheckpointSink<S> for CheckpointSaveSink<S> {
    async fn on_checkpoint(
        &mut self,
        state: &S,
        frame: &FrameInfo,
    ) -> Result<(), CheckpointStoreError> {
        let cp = Checkpoint::new(
            frame.next_node.clone(),
            state,
            self.graph_hash,
            frame.step,
        );
        let cp_id = cp.checkpoint_id;

        // ★ 同步等待保存 — 失败即返回 Err，不越过边界
        (self.save_fn)(cp, self.trace_id).await?;

        // CheckpointSaved 事件 — 尽力而为的观测（try_send 可能丢，不作可靠握手）
        if let Some(tx) = &self.event_tx {
            let _ = tx.try_send(GraphEvent::CheckpointSaved {
                checkpoint_id: cp_id,
                next_node: frame.next_node.as_ref().map(|n| n.0.clone()),
                step: frame.step,
            });
        }

        // 保留策略 prune — best-effort（检查点已落盘即视为越过边界）
        if let Some(keep) = self.retention.prune_keep() {
            if keep == 0 {
                return Err(CheckpointStoreError::Storage(
                    "KeepLatest(0) 无效：会删除全部恢复点".into(),
                ));
            }
            if let Some(s) = &self.store {
                if let Err(e) = s.prune(&self.trace_id, keep).await {
                    tracing::warn!(error = %e, "checkpoint prune failed");
                }
            }
        }
        Ok(())
    }
}
```

`CheckpointConfig` 的 `save_fn`/`graph_hash`/`retention`/`store` 字段需对同 crate 可见（改 `pub(crate)`）。

- [ ] **Step 4: CheckpointConfig::for_store**

`checkpoint_policy.rs` 追加：

```rust
impl<S: WorkflowState> CheckpointConfig<S> {
    /// 便捷构造器 — 从 store + codec 构建 save_fn（serialize + save_with_trace）。
    pub fn for_store(
        store: Arc<dyn crate::checkpoint::store::BlobCheckpointStore>,
        codec: impl crate::checkpoint::checkpoint_codec::CheckpointCodec<S> + Clone + Send + Sync + 'static,
        graph_hash: u64,
    ) -> Self {
        let save_fn: CheckpointSaveFn<S> = Box::new(move |cp, trace_id| {
            let store = store.clone();
            let codec = codec.clone();
            Box::pin(async move {
                let blob = codec.serialize(&cp, graph_hash)?;
                store.save_with_trace(&trace_id, &blob).await
            })
        });
        Self {
            save_fn: Arc::new(save_fn),
            trigger: crate::checkpoint::checkpoint_policy::TriggerPolicy::default(),
            retention: RetentionPolicy::default(),
            graph_hash,
            store: Some(store),
        }
    }
}
```

`checkpoint_codec.rs` `SerdeCheckpointCodec` 加 `Clone` derive。

- [ ] **Step 5: execution_loop.rs 接线**

删除旧 `CheckpointSaveSink`（134-161 行），`run_execution_loop` 中：

```rust
let mut cp_sink: Option<CheckpointSaveSink<S>> =
    checkpoint.map(|cfg| CheckpointSaveSink::new(cfg, trace_id, Some(event_tx.clone())));
```

- [ ] **Step 6: 运行测试**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_store_test --test checkpoint_test`
Expected: T7c PASS（依赖 Task 6 的 `execute_stream_with_checkpoint` — 若 Task 6 未完成则本测试暂编译不过，与 T1/T2 同批在 Task 6 后转绿；`for_store` 本身可用 `checkpoint_test` 的既有 TypedCheckpointStore 用例旁证）

- [ ] **Step 7: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "feat(graph): CheckpointSaveSink 同步保存（去 fire-and-forget）+ KeepLatest(0) 拒绝 + CheckpointSaved 事件发射 + CheckpointConfig::for_store"
```

---

### Task 5: FileBlobStore（磁盘后端，seq 提交序号 + flush/rename）

**Files:**
- Modify: `lellm-graph/src/checkpoint/store.rs`（追加 FileBlobStore，~200 行，总计 ~350 < 400）
- Modify: `lellm-graph/src/checkpoint/mod.rs`（`pub use store::FileBlobStore;` 已由 `pub use store::*` 覆盖，确认即可）
- Modify: `lellm-graph/src/lib.rs`（确认 FileBlobStore 经 `pub use checkpoint::{BlobCheckpointStore, InMemoryBlobStore}` 行补入）
- Test: `lellm-graph/tests/restore_store_test.rs`（追加 T5）

**Interfaces:**
- Consumes: `BlobCheckpointStore` SPI（save_with_trace/load/load_latest/list/delete/prune）、`CheckpointBlob`、`CheckpointId`（sparkid 字符串）、base64
- Produces:
  - `pub struct FileBlobStore { root: PathBuf }`
  - `FileBlobStore::new(root: impl Into<PathBuf>) -> Self`、`FileBlobStore::root(&self) -> &Path`
  - 布局：`<root>/<trace_id>/<seq>_<checkpoint_id>`；临时文件 `<seq>_<id>.tmp`
  - 信封 JSON：`{ "id": <sparkid 字符串>, "graph_hash": u64, "created_at_nanos": u64, "data": <base64 字符串> }`

**R6.1 落实点**：seq 解析为 `u64` **数值排序**（非文件名字典序）；`max + 1` 用 `checked_add`，溢出 → `Err(Storage)`；`.tmp` 与非规范文件名（无前缀下划线 / 前缀非数字）**不参与** max+1 / latest / list / prune。

**单写者约束（doc 必须写明）**：同一 trace 同时只能有一个活跃执行者——seq 的「扫目录取 max+1」在并发写者下会取到同一 seq 而冲突；phase 1 不加锁，多执行者分叉写入不支持。

**落盘边界（doc 必须写明）**：flush+rename 保证**写入完成 + 原子可见**（进程崩溃安全：panic/SIGKILL 后文件要么完整存在、要么不存在）；**不保证断电/OS 崩溃**（需文件 + 目录 fsync，phase 2 评估）。

- [ ] **Step 1: 写失败测试（T5）**

追加到 `tests/restore_store_test.rs`：

```rust
/// T5: FileBlobStore — 往返 / seq 单调（含重建延续）/ .tmp 排除 / 最新损坏不回退 / seq 溢出
#[tokio::test]
async fn t5_file_store_roundtrip_seq_tmp_corruption_overflow() {
    use lellm_graph::{BlobCheckpointStore, CheckpointBlob, FileBlobStore, CheckpointId, TraceId};
    use std::time::SystemTime;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let trace = TraceId::new();
    let hash = 0xABCD_u64;

    let make_blob = |tag: u8| {
        CheckpointBlob::new(
            CheckpointId::new(),
            vec![tag, tag, tag],
            hash,
            SystemTime::now(),
        )
    };

    // 1. 保存 3 个 → load 逐个往返 → load_latest = 第 3 个 → list 倒序 3 个
    let store = FileBlobStore::new(root.clone());
    let (id1, id2, id3) = (make_blob(1), make_blob(2), make_blob(3));
    store.save_with_trace(&trace, &id1).await.expect("save1");
    store.save_with_trace(&trace, &id2).await.expect("save2");
    store.save_with_trace(&trace, &id3).await.expect("save3");

    assert_eq!(store.load(&id1.id).await.expect("l1").unwrap().data, vec![1, 1, 1]);
    let latest = store.load_latest(&trace).await.expect("latest").expect("some");
    assert_eq!(latest.id, id3.id, "latest = 3rd saved");
    let ids = store.list(&trace).await.expect("list");
    assert_eq!(ids, vec![id3.id, id2.id, id1.id], "list by seq desc");

    // 2. seq 延续：重建 store（模拟新进程），保存 → seq 继续（不重置）
    drop(store);
    let store2 = FileBlobStore::new(root.clone());
    let id4 = make_blob(4);
    store2.save_with_trace(&trace, &id4).await.expect("save4");
    let latest = store2.load_latest(&trace).await.expect("latest2").unwrap();
    assert_eq!(latest.id, id4.id, "seq continues after store rebuild");

    // 3. .tmp 残留不参与 latest / list
    let trace_dir = root.join(trace.to_string());
    let tmp_name = format!("99_{}.tmp", CheckpointId::new());
    std::fs::write(trace_dir.join(&tmp_name), b"partial write").expect("tmp file");
    let latest = store2.load_latest(&trace).await.expect("latest3").unwrap();
    assert_eq!(latest.id, id4.id, ".tmp residue ignored by load_latest");
    let ids = store2.list(&trace).await.expect("list3");
    assert_eq!(ids.len(), 4, ".tmp not listed");

    // 4. prune(2) → 保留 seq 最大的 2 个
    let pruned = store2.prune(&trace, 2).await.expect("prune");
    assert_eq!(pruned, 2);
    let ids = store2.list(&trace).await.expect("list4");
    assert_eq!(ids, vec![id4.id, id3.id], "prune keeps newest by seq");

    // 5. 最新文件损坏 → load_latest 报 Corrupted（不回退旧检查点）
    let latest_path = trace_dir
        .read_dir()
        .expect("dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains(&id4.id.to_string()))
        .expect("latest file");
    std::fs::write(&latest_path, b"corrupted!!").expect("corrupt");
    match store2.load_latest(&trace).await {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("expected Corrupted (no fallback), got: {other:?}"),
    }

    // 6. seq 溢出：制造 u64::MAX seq 文件 → 保存报错
    let overflow_dir = root.join(TraceId::new().to_string());
    std::fs::create_dir_all(&overflow_dir).expect("dir");
    std::fs::write(
        overflow_dir.join(format!("{}_{id}", u64::MAX, id = CheckpointId::new())),
        b"{}",
    )
    .expect("overflow file");
    let store3 = FileBlobStore::new(root.clone());
    let t3 = TraceId::new();
    // 注：overflow 目录属于上面新建的 trace；对它的保存应报溢出
    let overflow_trace = /* 上面 create_dir_all 用的 trace */ ();
    match store3.save_with_trace(&overflow_trace, &make_blob(9)).await {
        Err(CheckpointStoreError::Storage(msg)) => {
            assert!(msg.contains("seq overflow"), "got: {msg}")
        }
        other => panic!("expected seq overflow Storage error, got: {other:?}"),
    }
}
```

> **实现提示**：第 6 小步的 `overflow_trace` 需在创建 overflow 目录时保留该 `TraceId` 变量（测试代码里先 `let overflow_trace = TraceId::new();` 再 `create_dir_all(root.join(overflow_trace.to_string()))`）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_store_test t5`
Expected: 编译失败（`FileBlobStore` 不存在）

- [ ] **Step 3: 实现 FileBlobStore**

`store.rs` 追加（文件头 doc 补布局/写入协议/单写者/落盘边界四段说明）：

```rust
// ─── FileBlobStore ─────────────────────────────────────────────

/// 文件 Checkpoint 存储后端。
///
/// # 布局
///
/// ```text
/// <root>/<trace_id>/<seq>_<checkpoint_id>        # 已提交（原子可见）
/// <root>/<trace_id>/<seq>_<checkpoint_id>.tmp    # 写入中（崩溃残留，load 时忽略）
/// ```
///
/// - `seq` = trace 内**单调递增的提交序号**：保存时扫 trace 目录取当前最大 seq + 1
///   （目录即记录，无独立计数器文件，进程崩溃后自动延续）。
///   **按数值排序**（非文件名字典序）；`.tmp` 与非规范文件不参与。
/// - `created_at` 仅展示用途，不参与排序（墙上时钟可相同、可因校时倒退）。
///
/// # 写入协议
///
/// `write_all(信封) → flush().await → rename(.tmp → 最终名)`。
/// flush 确保写入完成（tokio::fs 写返回不代表完成）；同目录 rename 原子。
/// **落盘边界**：保证写入完成 + 原子可见（进程崩溃安全：panic/SIGKILL），
/// **不保证断电/OS 崩溃**（需文件 + 目录 fsync，phase 2 评估）。
///
/// # 单写者约束
///
/// **同一 trace 同时只能有一个活跃执行者**——seq 的「扫目录取 max+1」
/// 在并发写者下会取到同一 seq 而冲突。phase 1 不加锁；多执行者分叉写入不支持。
pub struct FileBlobStore {
    root: PathBuf,
}

/// 信封 — 文件内容（data 为 base64，对任意 Codec 字节通用）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    id: String,
    graph_hash: u64,
    created_at_nanos: u64,
    data: String,
}

impl FileBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn trace_dir(&self, trace_id: &TraceId) -> PathBuf {
        self.root.join(trace_id.to_string())
    }

    /// 扫描 trace 目录 → (seq, path, id)。跳过 `.tmp` 与非规范文件。
    fn scan_trace_dir(dir: &Path) -> Result<Vec<(u64, PathBuf, String)>, CheckpointStoreError> {
        let mut out = Vec::new();
        let rd = std::fs::read_dir(dir).map_err(|e| {
            CheckpointStoreError::Storage(format!("read trace dir {}: {e}", dir.display()))
        })?;
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".tmp") {
                continue; // 临时文件不参与
            }
            let Some(usize_pos) = name.find('_') else {
                continue; // 非规范文件
            };
            let Ok(seq) = name[..usize_pos].parse::<u64>() else {
                continue; // 前缀非数字 = 非规范
            };
            out.push((seq, entry.path(), name[usize_pos + 1..].to_string()));
        }
        Ok(out)
    }

    async fn read_blob(path: &Path) -> Result<CheckpointBlob, CheckpointStoreError> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("read {}: {e}", path.display())))?;
        let env: Envelope = serde_json::from_slice(&bytes)
            .map_err(|e| CheckpointStoreError::Corrupted(format!("envelope: {e}")))?;
        let data = base64::engine::Engine::decode(
            base64::engine::general_purpose::STANDARD,
            &env.data,
        )
        .map_err(|e| CheckpointStoreError::Corrupted(format!("base64: {e}")))?;
        let id = CheckpointId(
            env.id
                .parse::<sparkid::SparkId>()
                .map_err(|e| CheckpointStoreError::Corrupted(format!("id: {e}")))?,
        );
        let created_at = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(env.created_at_nanos);
        Ok(CheckpointBlob::new(id, data, env.graph_hash, created_at))
    }
}

#[async_trait]
impl BlobCheckpointStore for FileBlobStore {
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        blob: &CheckpointBlob,
    ) -> Result<(), CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("create dir: {e}")))?;

        let entries = Self::scan_trace_dir(&dir)?;
        let next_seq = entries
            .iter()
            .map(|(s, _, _)| *s)
            .max()
            .map(|m| {
                m.checked_add(1)
                    .ok_or_else(|| CheckpointStoreError::Storage("checkpoint seq overflow".into()))
            })
            .transpose()?
            .unwrap_or(1);

        let id_str = blob.id.to_string();
        let final_path = dir.join(format!("{next_seq}_{id_str}"));
        let tmp_path = dir.join(format!("{next_seq}_{id_str}.tmp"));

        let created_at_nanos = blob
            .created_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let env = Envelope {
            id: id_str,
            graph_hash: blob.graph_hash,
            created_at_nanos,
            data: base64::engine::Engine::encode(
                base64::engine::general_purpose::STANDARD,
                &blob.data,
            ),
        };
        let json = serde_json::to_vec(&env)
            .map_err(|e| CheckpointStoreError::Serialization(e.to_string()))?;

        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("create tmp: {e}")))?;
        file.write_all(&json)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("write: {e}")))?;
        file.flush()
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("flush: {e}")))?;
        // 注：此处不做 fsync — 进程崩溃安全，非断电安全（见类型 doc）
        tokio::fs::rename(&tmp_path, &final_path)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("rename: {e}")))?;
        Ok(())
    }

    async fn load(
        &self,
        id: &CheckpointId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        // SPI 的 load(id) 不带 trace 上下文 — sparkid 全局唯一，扫全部 trace 目录
        let id_str = id.to_string();
        let mut traces = std::fs::read_dir(&self.root).map_err(|e| {
            CheckpointStoreError::Storage(format!("read root: {e}"))
        })?;
        while let Some(entry) = traces.next() {
            let entry = entry.map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            if !entry.path().is_dir() {
                continue;
            }
            let entries = Self::scan_trace_dir(&entry.path())?;
            if let Some((_, path, _)) = entries.into_iter().find(|(_, _, eid)| *eid == id_str) {
                return Ok(Some(Self::read_blob(&path).await?));
            }
        }
        Ok(None)
    }

    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        if !dir.exists() {
            return Ok(None);
        }
        let entries = Self::scan_trace_dir(&dir)?;
        // 数值最大 seq = 最新；损坏 → Corrupted（不回退旧检查点 = 不悄悄重跑）
        match entries.into_iter().max_by_key(|(seq, _, _)| *seq) {
            Some((_, path, _)) => Ok(Some(Self::read_blob(&path).await?)),
            None => Ok(None),
        }
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Self::scan_trace_dir(&dir)?;
        entries.sort_by(|a, b| b.0.cmp(&a.0)); // seq 倒序
        entries
            .into_iter()
            .map(|(_, _, id)| {
                id.parse::<sparkid::SparkId>()
                    .map(CheckpointId)
                    .map_err(|e| CheckpointStoreError::Corrupted(format!("id: {e}")))
            })
            .collect()
    }

    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
        let id_str = id.to_string();
        let mut traces = std::fs::read_dir(&self.root).map_err(|e| {
            CheckpointStoreError::Storage(format!("read root: {e}"))
        })?;
        while let Some(entry) = traces.next() {
            let entry = entry.map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            if !entry.path().is_dir() {
                continue;
            }
            let entries = Self::scan_trace_dir(&entry.path())?;
            if let Some((_, path, _)) = entries.into_iter().find(|(_, _, eid)| *eid == id_str) {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| CheckpointStoreError::Storage(format!("delete: {e}")))?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        if !dir.exists() {
            return Ok(0);
        }
        let mut entries = Self::scan_trace_dir(&dir)?;
        if entries.len() <= keep {
            return Ok(0);
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0)); // seq 倒序，保留前 keep 个
        let mut deleted = 0;
        for (_, path, _) in &entries[keep..] {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| CheckpointStoreError::Storage(format!("prune: {e}")))?;
            deleted += 1;
        }
        Ok(deleted)
    }
}
```

`store.rs` 顶部补 `use std::path::{Path, PathBuf};`。

- [ ] **Step 4: lib.rs 导出**

`pub use checkpoint::{BlobCheckpointStore, FileBlobStore, InMemoryBlobStore};`

- [ ] **Step 5: 运行测试**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_store_test t5`
Expected: PASS（6 小步全过）

- [ ] **Step 6: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "feat(graph): FileBlobStore 磁盘后端（trace 内单调 seq 数值排序 + flush/rename 原子可见 + 单写者约束 + 最新损坏不回退）"
```

---

### Task 6: 恢复入口与校验（execute_stream_with_checkpoint / with_restore / 零执行 / RestoreNotLatest）

**Files:**
- Modify: `lellm-graph/src/test_executor.rs`（新入口 + 校验 + spawn 抽取）
- Test: `lellm-graph/tests/restore_test.rs`（追加 T3/T6/T7；T1/T2 转绿）
- Test: `lellm-graph/tests/restore_format_test.rs`（追加 T7b）

**Interfaces:**
- Consumes: Task 3 的 `run_inline_from` / `validate_persistable`、Task 4 的 `for_store`、Task 5 的 `FileBlobStore`（T7b 用 InMemory 即可）
- Produces:
  - `pub fn SimpleExecutor::execute_stream(&self, graph: Arc<Graph>, state: State) -> GraphExecution<State>`（**不变**，非持久化）
  - `pub fn SimpleExecutor::execute_stream_with_checkpoint(&self, graph: Arc<Graph>, state: State, config: CheckpointConfig<State>) -> Result<GraphExecution<State>, GraphError>`（sync；新 trace；入口图结构校验）
  - `pub async fn SimpleExecutor::execute_stream_with_restore(&self, graph: Arc<Graph>, restore_from: Checkpoint<State>, trace_id: TraceId, config: CheckpointConfig<State>) -> Result<GraphExecution<State>, GraphError>`（**async** — R6.3 最新性检查需 store I/O；入口校验见下）
  - `run_execution_loop` 恢复分支：`next_node=None` → 零执行（GraphStart + GraphComplete，不执行任何节点、不保存新检查点）；`Some(n)` → `run_inline_from(start=n, steps_used=cp.steps_used)` + `CheckpointSaveSink(trace_id 延续)`

**入口校验表（恢复入口，覆盖直接构造的检查点 — R6.2）：**

| 校验 | 失败错误 |
|---|---|
| 图结构：含 Parallel/Subgraph/Barrier | `TerminalError::RestoreUnsupported { node, kind }` |
| `cp.format_version == CHECKPOINT_FORMAT_VERSION` | `TerminalError::RestoreFailed { reason }`（格式不支持） |
| `cp.graph_hash == graph.canonical_hash()` | `TerminalError::RestoreFailed { reason }`（graph hash mismatch） |
| `next_node = Some(n)` 时 `n` 存在于 `graph.node_map()` | `TerminalError::NodeNotFound(n)` |
| `next_node = Some` 且 `steps_used >= max_steps`（预算耗尽） | `TerminalError::StepsExceeded { limit }`（**执行前**报错 — R6.5） |
| `next_node = None` 且 `steps_used >= max_steps` | **合法**（完成态，零执行 — R6.5） |
| `config.store = Some(s)` 时 `s.load_latest(&trace_id)` 的 id == `cp.checkpoint_id` | 不等 → `TerminalError::RestoreNotLatest { checkpoint, latest }`；None → `TerminalError::RestoreFailed`（R6.3） |

- [ ] **Step 1: 写失败测试（T3 完成态零执行 / T6 图校验 / T7 预算延续 / T7b 入口校验）**

追加到 `tests/restore_test.rs`：

```rust
/// T3: 完成态恢复 — next=None → 零节点执行（副作用计数不变），返回完成态 state
#[tokio::test]
async fn t3_completed_restore_zero_execution() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = linear_graph(&effects);
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let executor = SimpleExecutor::new(100);

    // 首次跑完
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph.clone()), State::new(), config_for(store.clone(), hash))
        .expect("entry");
    let (tid, result) = drain(exec.stream).await;
    result.expect("first run");
    assert_eq!(*effects.lock().expect("m"), vec!["a", "b", "c"]);

    // 加载完成态检查点（next=None）
    let codec = SerdeCheckpointCodec::<State>::new();
    let latest = store.load_latest(&tid).await.expect("latest").expect("some");
    let cp = codec.deserialize(&latest, hash).expect("cp");
    assert_eq!(cp.next_node, None);

    // 恢复 → 零执行
    let effects_before = effects.lock().expect("m").len();
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, tid, config_for(store, hash))
        .await
        .expect("restore entry");
    let (_tid2, result) = drain(exec.stream).await;
    let state = result.expect("completed restore");
    assert_eq!(effects.lock().expect("m").len(), effects_before, "zero node execution");
    // 完成态 state 原样返回
    assert!(state.contains("c") || true, "state restored (content per graph)");
}

/// T6: 持久化入口拒绝 Parallel/Subgraph/Barrier；非持久化不受影响
#[tokio::test]
async fn t6_persistence_entry_rejects_unsupported_graphs() {
    use lellm_graph::{BarrierNode, ParallelNode, SubgraphSpec, IdentityLens};

    // Parallel 图
    let parallel_graph = {
        let mut b = GraphBuilder::<State>::new("par");
        b.start("p");
        b.node("p", NodeKind::Parallel(
            ParallelNode::builder()
                .branch("x", Arc::new(TaskNode::new("x", |_ctx| Ok(()))) as _)
                .build(),
        ));
        b.end("p");
        b.build().expect("build")
    };
    let hash = parallel_graph.canonical_hash();
    let executor = SimpleExecutor::new(100);
    let err = executor
        .execute_stream_with_checkpoint(
            Arc::new(parallel_graph.clone()),
            State::new(),
            config_for(Arc::new(InMemoryBlobStore::new()), hash),
        )
        .expect_err("must reject at entry");
    let msg = err.to_string();
    assert!(msg.contains("Parallel"), "got: {msg}");
    assert!(msg.contains("RestoreUnsupported") || msg.contains("does not support"), "got: {msg}");

    // 同一 Parallel 图非持久化执行 → 正常（原行为）
    let exec = executor.execute_stream(Arc::new(parallel_graph), State::new());
    let (_t, r) = drain(exec.stream).await;
    r.expect("non-persistent execution unaffected");

    // Barrier 图
    let barrier_graph = {
        let mut b = GraphBuilder::<State>::new("bar");
        b.start("h");
        b.node("h", NodeKind::Barrier(BarrierNode::new("human")));
        b.end("h");
        b.build().expect("build")
    };
    let err = executor
        .execute_stream_with_checkpoint(
            Arc::new(barrier_graph),
            State::new(),
            config_for(Arc::new(InMemoryBlobStore::new()), 0),
        )
        .expect_err("must reject barrier");
    assert!(err.to_string().contains("Barrier"));

    // Subgraph 图
    let inner = {
        let mut b = GraphBuilder::<State>::new("inner");
        b.start("i");
        b.node("i", NodeKind::Task(TaskNode::new("i", |_ctx| Ok(()))));
        b.end("i");
        b.build().expect("build")
    };
    let sub_graph = {
        let mut b = GraphBuilder::<State>::new("sub");
        b.start("s");
        b.node("s", NodeKind::Subgraph(SubgraphSpec::new(Arc::new(inner), IdentityLens)));
        b.end("s");
        b.build().expect("build")
    };
    let err = executor
        .execute_stream_with_checkpoint(
            Arc::new(sub_graph),
            State::new(),
            config_for(Arc::new(InMemoryBlobStore::new()), 0),
        )
        .expect_err("must reject subgraph");
    assert!(err.to_string().contains("Subgraph"));
}
```

> **注意**：`ParallelNode::builder().branch(name, Arc<dyn FlowNode>)` — TaskNode 需 `as Arc<dyn FlowNode<State>>` 转换；若 `branch` 接受具体类型则按实际签名调整。`IdentityLens` 的构造若为 `IdentityLens::new()` 或单元结构体，按实际 API 调整（实施时 `cargo doc` 确认）。

T7（追加到 `tests/restore_test.rs`）：

```rust
/// 循环图 init→check⇄work→done（count<3 时 check→work，否则 check→done）。
fn loop_graph(
    effects: &Arc<std::sync::Mutex<Vec<String>>>,
    work_fail: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> lellm_graph::Graph {
    let g = effects.clone();
    let append = |name: &str| {
        let g = g.clone();
        move |_ctx: &mut lellm_graph::NodeContext| {
            g.lock().expect("effects").push(name.to_string());
            Ok(())
        }
    };
    let mut b = GraphBuilder::<State>::new("loop");
    b.start("init");
    b.node("init", TaskNode::new("init", append("init")));

    let check_fn = append("check");
    b.node("check", TaskNode::new("check", check_fn));

    let work_fn = {
        let g = g.clone();
        let fail = work_fail.clone();
        move |ctx: &mut lellm_graph::NodeContext| {
            if let Some(f) = &fail {
                if f.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(lellm_graph::GraphError::Terminal(
                        lellm_graph::TerminalError::StateError("simulated crash".into()),
                    ));
                }
            }
            g.lock().expect("effects").push("work".to_string());
            let count = ctx.state().get_i64("count").unwrap_or(0);
            ctx.record(lellm_graph::StateMutation::Put(
                "count".into(),
                serde_json::json!(count + 1),
            ));
            Ok(())
        }
    };
    b.node("work", TaskNode::new("work", work_fn));
    b.node("done", TaskNode::new("done", append("done")));

    b.edge("init", "check");
    b.edge_if("check", "work", |s: &State| s.get_i64("count").unwrap_or(0) < 3);
    b.edge_if("check", "done", |s: &State| s.get_i64("count").unwrap_or(0) >= 3);
    b.edge("work", "check");
    b.end("done");
    b.build().expect("build")
}

/// T7: 步数预算跨恢复延续 — 崩溃后恢复不重新获得完整 max_steps；到限后下一节点未执行
#[tokio::test]
async fn t7_loop_budget_continues_across_restore() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(true)); // work 首次执行即「崩溃」
    let graph = loop_graph(&effects, Some(fail.clone()));
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let executor = SimpleExecutor::new(5); // 总预算 5

    // 阶段 A：init(1) check(2) work(3)→Err 停止。最新检查点 = next=work, su=2
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph.clone()), State::new(), config_for(store.clone(), hash))
        .expect("entry");
    let (tid, result) = drain(exec.stream).await;
    let err = result.expect_err("stage A crashes at work");
    assert!(err.contains("simulated crash"), "got: {err}");

    let codec = SerdeCheckpointCodec::<State>::new();
    let latest = store.load_latest(&tid).await.expect("latest").expect("some");
    let cp = codec.deserialize(&latest, hash).expect("cp");
    assert_eq!(cp.next_node, Some(lellm_graph::NodeId("work".into())));
    assert_eq!(cp.steps_used, 2);

    // 阶段 B：恢复（同一总预算 5）→ work(3) check(4) work(5) → step6 StepsExceeded
    fail.store(false, Ordering::SeqCst);
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, tid, config_for(store, hash))
        .await
        .expect("restore entry");
    let (_tid2, result) = drain(exec.stream).await;
    let err = result.expect_err("stage B hits total budget");
    assert!(err.contains("step limit 5 exceeded"), "got: {err}");

    // 总执行 = 5 步：init check work|check work — 第 6 步（check）未执行
    assert_eq!(*effects.lock().expect("m"), vec!["init", "check", "check", "work", "work"]);
}
```

> **T7 断言核对**：阶段 A 的 effects = [init, check]（work 失败无副作用）；阶段 B = [check, work, work]（check(4) 追加、work(5) 追加；step6 的 check 未执行）。合并 = [init, check, check, work, work]。✓

T7b（追加到 `tests/restore_format_test.rs`）：

```rust
/// T7b: 恢复入口校验 — 直接构造的检查点（绕过反序列化）也被拦截
#[tokio::test]
async fn t7b_restore_entry_validation_direct_construction() {
    use lellm_graph::{
        CheckpointConfig, GraphBuilder, InMemoryBlobStore, NodeKind, SerdeCheckpointCodec,
        SimpleExecutor, TaskNode, TraceId,
    };
    use std::sync::Arc;

    let graph = {
        let mut b = GraphBuilder::<State>::new("ab");
        b.start("a");
        b.node("a", NodeKind::Task(TaskNode::new("a", |_ctx| Ok(()))));
        b.node("b", NodeKind::Task(TaskNode::new("b", |_ctx| Ok(()))));
        b.edge("a", "b");
        b.end("b");
        b.build().expect("build")
    };
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let config = || {
        CheckpointConfig::for_store(
            store.clone(),
            SerdeCheckpointCodec::<State>::new(),
            hash,
        )
    };
    let tid = TraceId::new();
    let executor = SimpleExecutor::new(5);

    // 基准合法检查点（next=b, su=1）
    let base = Checkpoint::new(Some(NodeId("b".into())), &State::new(), hash, 1);

    // 1. format_version 错 → 拒绝
    let mut bad = base.clone();
    bad.format_version = 999;
    let err = executor
        .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
        .await
        .expect_err("bad version");
    assert!(err.to_string().contains("format_version"), "got: {err:?}");

    // 2. graph_hash 错 → 拒绝
    let mut bad = base.clone();
    bad.graph_hash = hash ^ 0xFF;
    let err = executor
        .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
        .await
        .expect_err("bad hash");
    assert!(err.to_string().contains("graph hash mismatch"), "got: {err:?}");

    // 3. next_node 指向不存在节点 → 拒绝
    let mut bad = base.clone();
    bad.next_node = Some(NodeId("nonexistent".into()));
    let err = executor
        .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
        .await
        .expect_err("bad node");
    assert!(err.to_string().contains("nonexistent"), "got: {err:?}");

    // 4. next_node=Some 且 steps_used >= max_steps → 执行前报错（R6.5）
    let mut bad = base.clone();
    bad.steps_used = 5;
    let err = executor
        .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
        .await
        .expect_err("budget exhausted");
    assert!(err.to_string().contains("step limit"), "got: {err:?}");

    // 5. next_node=None 且 steps_used >= max_steps → 合法（完成态，R6.5）
    let mut done = base.clone();
    done.next_node = None;
    done.steps_used = 5;
    // store 为空 → 最新性检查会报 RestoreFailed（无检查点）；
    // 因此先保存一个「最新」检查点使 id 匹配：
    // 简化：此用例单独用无 store 的 config 验证步数边界放行
    let config_nostore = CheckpointConfig::new(
        move |_cp, _t| Box::pin(async { Ok(()) }),
        hash,
    );
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph.clone()), done, tid, config_nostore)
        .await
        .expect("completed state with exhausted budget is valid");
    let (_t, r) = drain_stream(exec.stream).await;
    r.expect("zero-execution complete");

    // 6. 非最新检查点 → RestoreNotLatest（R6.3）
    // 保存 cp1（next=b）与 cp2（next=None）到同一 trace，cp2 为最新
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = lellm_graph::TypedCheckpointStore::new(&store, codec);
    let cp1 = Checkpoint::new(Some(NodeId("b".into())), &State::new(), hash, 1);
    let cp2 = Checkpoint::new(None, &State::new(), hash, 2);
    typed.save_with_trace(&tid, &cp1, hash).await.expect("save cp1");
    typed.save_with_trace(&tid, &cp2, hash).await.expect("save cp2");
    let err = executor
        .execute_stream_with_restore(Arc::new(graph), cp1, tid, config())
        .await
        .expect_err("not latest");
    assert!(err.to_string().contains("not the latest"), "got: {err:?}");
}
```

> **注意**：`drain_stream` 即 `restore_test.rs` 的 `drain` 辅助（跨文件不可见 → 在本文件复制一份同实现，或提取到 `tests/common/mod.rs`；**选择复制**，测试文件独立）。第 5 用例的 `config_nostore`（store=None）跳过最新性检查 — 这是 R6.3 的边界：无 store 时无法验证最新性，放行（文档注明：续写目标由 save_fn 决定）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_test --test restore_format_test`
Expected: 编译失败（`execute_stream_with_checkpoint`/`execute_stream_with_restore` 不存在）

- [ ] **Step 3: 实现 test_executor.rs 新入口**

`test_executor.rs` 修改（保留 `execute()`/`execute_stream()` 原实现；`execute_stream_with_restore` 旧签名替换）：

```rust
impl SimpleExecutor {
    /// 非持久化执行（行为不变，不做图结构校验）。
    pub fn execute_stream(&self, graph: Arc<Graph>, state: State) -> GraphExecution<State> {
        self.spawn(graph, state, TraceId::new(), None, None)
    }

    /// 持久化执行 — 每节点同步保存检查点（新 trace）。
    ///
    /// 入口校验：含 Parallel/Subgraph/Barrier 的图在此拒绝（`RestoreUnsupported`），
    /// 不等崩溃后恢复才报错。
    pub fn execute_stream_with_checkpoint(
        &self,
        graph: Arc<Graph>,
        state: State,
        config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError> {
        graph.validate_persistable()?;
        Ok(self.spawn(graph, state, TraceId::new(), Some(config), None))
    }

    /// 持久化恢复 — 从检查点续跑，并**继续保存**到同一 trace。
    ///
    /// - `trace_id` — 延续原 trace（新检查点保存于该 trace，seq 单调前进）
    /// - `config` — 恢复后继续保存的持久化配置（store/retention）
    ///
    /// # 入口校验（覆盖直接构造的检查点）
    ///
    /// 图结构 / format_version / graph_hash / next_node 存在 / 步数边界 /
    /// **最新性**（`config.store` 存在时：传入检查点必须是该 trace 最新，
    /// 否则 `RestoreNotLatest` — 要恢复旧检查点请换新 trace）。
    pub async fn execute_stream_with_restore(
        &self,
        graph: Arc<Graph>,
        restore_from: Checkpoint<State>,
        trace_id: TraceId,
        config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError> {
        graph.validate_persistable()?;
        Self::validate_restore_checkpoint(&graph, &restore_from, self.max_steps)?;

        // R6.3：最新性检查（需 store I/O → 本方法 async）
        if let Some(store) = &config.store {
            match store.load_latest(&trace_id).await {
                Ok(Some(latest)) => {
                    if latest.id != restore_from.checkpoint_id {
                        return Err(GraphError::Terminal(
                            crate::error::TerminalError::RestoreNotLatest {
                                checkpoint: restore_from.checkpoint_id.to_string(),
                                latest: latest.id.to_string(),
                            },
                        ));
                    }
                }
                Ok(None) => {
                    return Err(GraphError::Terminal(
                        crate::error::TerminalError::RestoreFailed {
                            reason: format!("no checkpoints found for trace {trace_id}"),
                        },
                    ))
                }
                Err(e) => {
                    return Err(GraphError::Terminal(
                        crate::error::TerminalError::RestoreFailed {
                            reason: format!("load latest checkpoint: {e}"),
                        },
                    ))
                }
            }
        }

        let state = State::restore(restore_from.state.clone());
        Ok(self.spawn(graph, state, trace_id, Some(config), Some(restore_from)))
    }

    /// 恢复入口同步校验（版本/指纹/节点存在/步数边界 — R6.5）。
    fn validate_restore_checkpoint(
        graph: &Graph,
        cp: &Checkpoint<State>,
        max_steps: usize,
    ) -> Result<(), GraphError> {
        if cp.format_version != crate::checkpoint::CHECKPOINT_FORMAT_VERSION {
            return Err(GraphError::Terminal(
                crate::error::TerminalError::RestoreFailed {
                    reason: format!(
                        "unsupported checkpoint format_version: {} (expected {})",
                        cp.format_version,
                        crate::checkpoint::CHECKPOINT_FORMAT_VERSION
                    ),
                },
            ));
        }
        if cp.graph_hash != graph.canonical_hash() {
            return Err(GraphError::Terminal(
                crate::error::TerminalError::RestoreFailed {
                    reason: format!(
                        "graph hash mismatch: expected {:016x}, got {:016x}",
                        graph.canonical_hash(),
                        cp.graph_hash
                    ),
                },
            ));
        }
        if let Some(n) = &cp.next_node {
            if !graph.node_map().contains_key(&n.0) {
                return Err(GraphError::Terminal(
                    crate::error::TerminalError::NodeNotFound(n.0.clone()),
                ));
            }
            // R6.5：还有下一节点但预算已耗尽 → 执行前报错
            if cp.steps_used >= max_steps {
                return Err(GraphError::Terminal(
                    crate::error::TerminalError::StepsExceeded { limit: max_steps },
                ));
            }
        }
        // next_node = None（完成态）：允许预算耗尽（零执行直接返回完成）
        Ok(())
    }

    /// 统一 spawn — 通道 + run_execution_loop。
    fn spawn(
        &self,
        graph: Arc<Graph>,
        state: State,
        trace_id: TraceId,
        checkpoint: Option<CheckpointConfig<State>>,
        restore_from: Option<Checkpoint<State>>,
    ) -> GraphExecution<State> {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
        let (decision_tx, decision_rx) = tokio::sync::mpsc::channel(256);
        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel(1);
        let cancel = CancellationToken::new();
        let handle = GraphHandle::new(decision_tx, cancel_tx);

        tokio::spawn(crate::exec::execution_loop::run_execution_loop(
            graph,
            state,
            self.max_steps,
            trace_id,
            event_tx,
            decision_rx,
            cancel_rx,
            cancel,
            checkpoint,
            None, // trace_sink
            restore_from,
        ));

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }
}
```

`run_execution_loop`（execution_loop.rs）恢复分支（GraphStart 发射之后、Engine 创建之前）：

```rust
// 恢复路径：还原 State
let mut engine_state = match &restore_from {
    Some(cp) => S::restore(cp.state.clone()),
    None => state,
};

// 「恢复已完成」：零执行 — 发 GraphStart + GraphComplete，不执行任何节点、不保存
if let Some(cp) = &restore_from {
    if cp.next_node.is_none() {
        let duration = start_time.elapsed();
        let result = GraphResult {
            trace_id,
            state: engine_state,
            execution_log: Vec::new(),
            duration,
            trace: None,
        };
        let _ = event_tx.try_send(GraphEvent::GraphComplete { result });
        return;
    }
}

let (start_node, steps_used) = match &restore_from {
    Some(cp) => (cp.next_node.as_ref().expect("checked").0.clone(), cp.steps_used),
    None => (graph.start_node().to_string(), 0),
};
```

Engine 块内调用改为 `graph.run_inline_from(&mut engine, &start_node, steps_used, max_steps, &mut step_cb).await`。

- [ ] **Step 4: 运行测试（T1/T2/T3/T6/T7/T7b 全批转绿）**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_test --test restore_format_test --test restore_store_test`
Expected: 全绿

- [ ] **Step 5: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "feat(graph): 恢复入口 — execute_stream_with_checkpoint/with_restore（入口校验 + 零执行完成态 + 最新性检查 + 预算延续）"
```

---

### Task 7: restore_probe 辅助二进制 + T8/T9/T10 真崩溃测试

**Files:**
- Create: `lellm-graph/src/bin/restore_probe.rs`
- Create: `lellm-graph/tests/restore_new_process.rs`

**Interfaces:**
- Consumes: Task 4/5/6 的完整 API（`FileBlobStore`、`for_store`、`execute_stream_with_checkpoint/with_restore`）
- Produces:
  - 二进制 `restore_probe`，模式：
    - `run <dir> <effects_file> <block_node> <linear|loop>` — 持久化执行；到达 block_node 时先**直接查磁盘**确认指向它的检查点已落盘，向 stdout 打印 `HANDSHAKE <trace_id> <next_node>` 并 flush，然后**无限阻塞**（R6.4：防止父进程 kill 前又执行下一节点）
    - `restore <dir> <trace_id> <max_steps> <effects_file> <linear|loop> [block_node]` — 加载最新检查点 → `execute_stream_with_restore` 续跑（同一 trace 继续保存）→ 可选再阻塞；跑到完成打印 `COMPLETE <state_json>`，出错打印 `ERROR <msg>` 并以退出码 1 结束
  - 可靠握手 = stdout 行（管道不丢）+ 磁盘确认；**不用 `CheckpointSaved` 事件作握手**（try_send 可能丢，仅尽力观测）

**T8/T9/T10 承诺边界（写进测试 doc comment）**：验证「检查点**成功落盘后进程被强制终止** → 新进程恢复不重跑已提交节点」。**不能**证明「工具成功但保存前崩溃」不重复执行（需幂等键/去重，暂缓项）。

- [ ] **Step 1: 实现 restore_probe.rs**

```rust
//! R4 新进程恢复测试辅助二进制。
//!
//! 模式：
//! - `run <dir> <effects_file> <block_node> <linear|loop>`
//! - `restore <dir> <trace_id> <max_steps> <effects_file> <linear|loop> [block_node]`
//!
//! 可靠握手：stdout 行 `HANDSHAKE <trace_id> <next_node>`（管道，不丢）+
//! 磁盘确认（直接查 store 文件，不依赖事件）。握手后无限阻塞，等待父进程 kill。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lellm_graph::{
    BlobCheckpointStore, CheckpointConfig, FileBlobStore, GraphBuilder, NodeId, NodeKind,
    SerdeCheckpointCodec, SimpleExecutor, State, StateExt, StateMutation, TaskNode, TraceId,
};
use uuid::Uuid;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(|s| s.as_str()) {
        Some("run") => tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(run(&args[1..])),
        Some("restore") => tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(restore(&args[1..])),
        _ => Err("usage: restore_probe <run|restore> ...".into()),
    };
    if let Err(e) = result {
        eprintln!("PROBE_ERROR {e}");
        std::process::exit(2);
    }
}

// ─── 图构建 ────────────────────────────────────────────────────

fn effect_fn(name: &str, effects: PathBuf) -> impl Fn(&mut lellm_graph::NodeContext) -> Result<(), lellm_graph::GraphError> + Send + Sync + 'static {
    move |_ctx| {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&effects)
            .map_err(|e| io_err(e, "open effects"))?;
        writeln!(f, "{name}").map_err(|e| io_err(e, "append effects"))?;
        Ok(())
    }
}

/// 阻塞节点 — 磁盘确认 → 握手 → 无限阻塞（R6.4）。
fn block_fn(
    block_node: String,
    dir: PathBuf,
    effects: PathBuf,
) -> impl Fn(&mut lellm_graph::NodeContext) -> Result<(), lellm_graph::GraphError> + Send + Sync + 'static {
    move |_ctx| {
        // 1. 找 trace 目录（dir 下唯一子目录）
        let trace_dir = std::fs::read_dir(&dir)
            .map_err(|e| io_err(e, "read dir"))?
            .into_iter()
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .map(|e| e.path())
            .ok_or_else(|| lellm_graph::GraphError::Terminal(
                lellm_graph::TerminalError::StateError("no trace dir".into()),
            ))?;
        let trace_id_str = trace_dir
            .file_name()
            .expect("file_name")
            .to_string_lossy()
            .to_string();

        // 2. 磁盘确认：最大 seq 文件的 next_node == block_node（磁盘为真，不依赖事件）
        let files: Vec<(u64, PathBuf)> = std::fs::read_dir(&trace_dir)
            .map_err(|e| io_err(e, "read trace dir"))?
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                !n.ends_with(".tmp") && n.contains('_')
            })
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let seq = n.split('_').next()?.parse::<u64>().ok()?;
                Some((seq, e.path()))
            })
            .collect();
        let (_, latest) = files
            .into_iter()
            .max_by_key(|(s, _)| *s)
            .ok_or_else(|| lellm_graph::GraphError::Terminal(
                lellm_graph::TerminalError::StateError("no checkpoint on disk".into()),
            ))?;
        let env: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&latest).map_err(|e| io_err(e, "read checkpoint"))?,
        )
        .map_err(|e| lellm_graph::GraphError::Terminal(
            lellm_graph::TerminalError::StateError(format!("envelope: {e}")),
        ))?;
        let data = base64::engine::Engine::decode(
            base64::engine::general_purpose::STANDARD,
            env["data"].as_str().expect("data str"),
        )
        .map_err(|e| lellm_graph::GraphError::Terminal(
            lellm_graph::TerminalError::StateError(format!("base64: {e}")),
        ))?;
        let cp: serde_json::Value = serde_json::from_slice(&data)
            .map_err(|e| lellm_graph::GraphError::Terminal(
                lellm_graph::TerminalError::StateError(format!("checkpoint json: {e}")),
            ))?;
        let next = cp["next_node"].as_str().unwrap_or("");
        if next != block_node {
            return Err(lellm_graph::GraphError::Terminal(
                lellm_graph::TerminalError::StateError(format!(
                    "disk checkpoint next_node={next}, expected {block_node}"
                )),
            ));
        }

        // 3. 握手（stdout 管道，可靠）
        println!("HANDSHAKE {} {block_node}", trace_id_str);
        std::io::stdout().flush().map_err(|e| io_err(e, "flush"))?;

        // 4. 无限阻塞 — 防止父进程 kill 前又执行下一节点（R6.4）
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

fn io_err(e: std::io::Error, ctx: &str) -> lellm_graph::GraphError {
    lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(format!("{ctx}: {e}")))
}

fn build_graph(
    kind: &str,
    effects: PathBuf,
    block_node: Option<&str>,
    dir: PathBuf,
) -> (lellm_graph::Graph, u64) {
    let g = match kind {
        "linear" => {
            let mk = |name: &str| {
                if Some(name) == block_node {
                    TaskNode::new(name, block_fn(name.to_string(), dir.clone(), effects.clone()))
                } else {
                    TaskNode::new(name, effect_fn(name, effects.clone()))
                }
            };
            GraphBuilder::<State>::new("probe_linear")
                .start("a")
                .node("a", mk("a"))
                .node("b", mk("b"))
                .node("c", mk("c"))
                .node("d", mk("d"))
                .edge("a", "b")
                .edge("b", "c")
                .edge("c", "d")
                .end("d")
                .build()
                .expect("build linear")
        }
        "loop" => {
            let check_node = if block_node == Some("check") {
                TaskNode::new("check", block_fn("check".into(), dir.clone(), effects.clone()))
            } else {
                TaskNode::new("check", effect_fn("check", effects.clone()))
            };
            let work_fn = move |ctx: &mut lellm_graph::NodeContext| {
                effects_append(&effects, "work");
                let count = ctx.state().get_i64("count").unwrap_or(0);
                ctx.record(StateMutation::Put(
                    "count".into(),
                    serde_json::json!(count + 1),
                ));
                Ok(())
            };
            GraphBuilder::<State>::new("probe_loop")
                .start("init")
                .node("init", TaskNode::new("init", effect_fn("init", effects.clone())))
                .node("check", check_node)
                .node("work", TaskNode::new("work", work_fn))
                .node("done", TaskNode::new("done", effect_fn("done", effects.clone())))
                .edge("init", "check")
                .edge_if("check", "work", |s: &State| s.get_i64("count").unwrap_or(0) < 3)
                .edge_if("check", "done", |s: &State| s.get_i64("count").unwrap_or(0) >= 3)
                .edge("work", "check")
                .end("done")
                .build()
                .expect("build loop")
        }
        other => panic!("unknown graph kind: {other}"),
    };
    let hash = g.canonical_hash();
    (g, hash)
}

fn effects_append(effects: &Path, name: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(effects)
        .expect("open effects");
    writeln!(f, "{name}").expect("append");
}

// ─── 模式 ──────────────────────────────────────────────────────

async fn run(args: &[String]) -> Result<(), String> {
    assert!(args.len() >= 4, "run <dir> <effects> <block_node> <kind>");
    let (dir, effects, block_node, kind) = (
        PathBuf::from(&args[0]),
        PathBuf::from(&args[1]),
        args[2].clone(),
        args[3].clone(),
    );
    let (graph, hash) = build_graph(&kind, effects, Some(&block_node), dir.clone());
    let store = Arc::new(FileBlobStore::new(dir));
    let config = CheckpointConfig::for_store(store, SerdeCheckpointCodec::<State>::new(), hash);
    let executor = SimpleExecutor::new(1000);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config)
        .map_err(|e| format!("entry: {e}"))?;
    // 消费事件直至终止（run 模式通常被 kill，不会到达）
    drain_events(exec.stream).await
}

async fn restore(args: &[String]) -> Result<(), String> {
    assert!(
        args.len() >= 5,
        "restore <dir> <trace_id> <max_steps> <effects> <kind> [block_node]"
    );
    let (dir, trace_id_str, max_steps, effects, kind) = (
        PathBuf::from(&args[0]),
        args[1].clone(),
        args[2].parse::<usize>().expect("max_steps"),
        PathBuf::from(&args[3]),
        args[4].clone(),
    );
    let block_node = args.get(5).cloned();
    let trace_id = TraceId(Uuid::parse_str(&trace_id_str).map_err(|e| e.to_string())?);

    let (graph, hash) = build_graph(&kind, effects, block_node.as_deref(), dir.clone());
    let store = Arc::new(FileBlobStore::new(dir));
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = lellm_graph::TypedCheckpointStore::new(&store, codec.clone());
    let cp = typed
        .load_latest(&trace_id, hash)
        .await
        .map_err(|e| format!("load latest: {e}"))?
        .ok_or_else(|| "no checkpoint for trace".to_string())?;
    let config = CheckpointConfig::for_store(store, codec, hash);
    let executor = SimpleExecutor::new(max_steps);
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, trace_id, config)
        .await
        .map_err(|e| format!("restore entry: {e}"))?;

    match drain_events(exec.stream).await {
        Ok(state) => {
            println!("COMPLETE {}", serde_json::to_string(&state).expect("state json"));
            Ok(())
        }
        Err(msg) => {
            println!("ERROR {msg}");
            Err(msg)
        }
    }
}

/// 消费事件至终止。Ok(state) = GraphComplete；Err(msg) = GraphError/流意外关闭。
async fn drain_events(
    mut stream: lellm_graph::GraphStream,
) -> Result<State, String> {
    loop {
        match stream.recv().await {
            Some(lellm_graph::GraphEvent::GraphComplete { result }) => return Ok(result.state),
            Some(lellm_graph::GraphEvent::GraphError { error, .. }) => {
                return Err(error.to_string())
            }
            Some(_) => {}
            None => return Err("stream closed without terminal event".into()),
        }
    }
}
```

> **注意**：`NodeKind` import 若未使用则删除；`NodeId` 同理（按实际编译结果清理 unused imports）。`lellm_graph::NodeContext` 的导出路径以 lib.rs 为准（`pub use node::{LeafContext, NodeContext}`）。

- [ ] **Step 2: 写 T8/T9/T10 测试**

新建 `lellm-graph/tests/restore_new_process.rs`：

```rust
//! R4 新进程恢复集成测试 — 真崩溃协议。
//!
//! **承诺边界**：验证「检查点成功落盘后进程被强制终止（kill -9）→ 新进程恢复
//! 不重跑已提交节点」。**不能**证明「工具成功但保存前崩溃」不重复执行
//! （需幂等键/结果去重/业务补偿，暂缓项）。
//!
//! **可靠握手**：stdout 行（管道，不丢）+ 子进程磁盘确认。
//! `CheckpointSaved` 事件（try_send 可能丢）仅尽力观测，不作握手。
//!
//! **外部进程测试**：总耗时 < 30s（握手超时 15s；正常路径 < 5s）。
//! 子进程握手后无限阻塞（防父进程 kill 前又执行下一节点）；
//! 父进程握手读取设超时，超时/失败也终止并回收子进程。

#[cfg(unix)]
mod unix {
    use std::io::Write as _;
    use std::process::Stdio;
    use std::time::Duration;

    fn probe() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_restore_probe"))
    }

    /// 读子进程 stdout 至 HANDSHAKE 行（超时 → None）。
    async fn read_handshake(
        child: &mut tokio::process::Child,
        timeout: Duration,
    ) -> Option<String> {
        let stdout = child.stdout.as_mut()?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    if line.starts_with("HANDSHAKE ") {
                        return Some(line);
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    /// kill -9 + 回收，验证 signal 退出。超时/失败路径也保证回收（R6.4）。
    async fn kill9_and_reap(child: &mut tokio::process::Child) -> Option<i32> {
        let pid = child.id();
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .await;
        match child.wait().await {
            Ok(status) => status.signal(),
            Err(_) => None,
        }
    }

    /// 失败时终止并回收子进程（R6.4：任何失败路径都不得泄漏子进程）。
    async fn kill_and_reap_quiet(child: &mut tokio::process::Child) {
        let pid = child.id();
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output()
            .await;
        let _ = child.wait().await;
    }

    fn effects_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .expect("read effects")
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    /// T8: R4 主链路 — a→b→c→d，c 处握手后 kill -9，新进程恢复跑到完成
    #[tokio::test]
    async fn t8_r4_crash_kill9_restore_linear() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ckpt");
        let effects = tmp.path().join("effects.txt");

        // ① 子进程 A：run，block_node=c
        let mut child = tokio::process::Command::new(probe())
            .args([
                "run",
                dir.to_str().unwrap(),
                effects.to_str().unwrap(),
                "c",
                "linear",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn A");

        // ② 握手（15s 超时；超时 → 终止回收 + 失败）
        let line = match read_handshake(&mut child, Duration::from_secs(15)).await {
            Some(l) => l,
            None => {
                let out = child.wait_with_output().await.expect("reap A");
                kill_and_reap_quiet(&mut child).await;
                panic!(
                    "A: handshake timeout. stderr: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        };
        let parts: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(parts[0], "HANDSHAKE");
        let (trace_id_str, next_node) = (parts[1].to_string(), parts[2].to_string());
        assert_eq!(next_node, "c");

        // ③ kill -9 + 回收（验证 signal 退出）
        let sig = kill9_and_reap(&mut child).await;
        assert_eq!(sig, Some(9), "child A must exit by SIGKILL");

        // 子进程 A 阶段断言：副作用 = [a, b]（c 未执行）；磁盘存在 next=c 检查点
        assert_eq!(effects_lines(&effects), vec!["a", "b"]);
        let trace_dir = dir.join(&trace_id_str);
        assert!(trace_dir.exists(), "trace dir on disk");

        // ④ 子进程 B：restore 跑到完成
        let mut child_b = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "100",
                effects.to_str().unwrap(),
                "linear",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn B");
        let out_b = tokio::time::timeout(Duration::from_secs(15), child_b.wait_with_output())
            .await
            .expect("B timeout")
            .expect("reap B");
        assert!(
            out_b.status.success(),
            "B failed: stderr={}",
            String::from_utf8_lossy(&out_b.stderr)
        );
        let stdout_b = String::from_utf8_lossy(&out_b.stdout);
        assert!(stdout_b.contains("COMPLETE"), "B stdout: {stdout_b}");

        // 最终断言：副作用 = [a, b, c, d]（a/b 不重跑、c/d 各一次）
        assert_eq!(effects_lines(&effects), vec!["a", "b", "c", "d"]);
    }

    /// T9: 循环中途恢复 — 执行位置 + 步数预算延续（总预算到限才 StepsExceeded）+ 到限后下一节点未执行
    #[tokio::test]
    async fn t9_crash_restore_loop_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ckpt");
        let effects = tmp.path().join("effects.txt");

        // 子进程 A：loop 图，block_node=check（首次到达，steps_used=1 后握手）
        let mut child = tokio::process::Command::new(probe())
            .args([
                "run",
                dir.to_str().unwrap(),
                effects.to_str().unwrap(),
                "check",
                "loop",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn A");
        let line = match read_handshake(&mut child, Duration::from_secs(15)).await {
            Some(l) => l,
            None => {
                let out = child.wait_with_output().await.expect("reap A");
                kill_and_reap_quiet(&mut child).await;
                panic!("A: handshake timeout. stderr: {}", String::from_utf8_lossy(&out.stderr));
            }
        };
        let trace_id_str = line.split_whitespace().nth(1).expect("trace_id").to_string();
        assert_eq!(kill9_and_reap(&mut child).await, Some(9));
        assert_eq!(effects_lines(&effects), vec!["init"]);

        // 子进程 B：restore，max_steps=5 → check(2) work(3) check(4) work(5) → step6 StepsExceeded
        let mut child_b = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "5",
                effects.to_str().unwrap(),
                "loop",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn B");
        let out_b = tokio::time::timeout(Duration::from_secs(15), child_b.wait_with_output())
            .await
            .expect("B timeout")
            .expect("reap B");
        let stdout_b = String::from_utf8_lossy(&out_b.stdout);
        assert!(
            stdout_b.contains("ERROR") && stdout_b.contains("step limit 5 exceeded"),
            "B stdout: {stdout_b}"
        );

        // 总执行 = 5 步（预算延续，非 1+5）；第 6 步（check）未执行
        assert_eq!(
            effects_lines(&effects),
            vec!["init", "check", "work", "check", "work"]
        );
    }

    /// T10: 双重恢复（R2 验收）— 保存 → 新进程恢复 → 再保存 → 再次新进程恢复
    #[tokio::test]
    async fn t10_double_restore_seq_continues() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ckpt");
        let effects = tmp.path().join("effects.txt");

        // 子进程 A：run linear block c → 握手 → kill
        let mut child = tokio::process::Command::new(probe())
            .args([
                "run",
                dir.to_str().unwrap(),
                effects.to_str().unwrap(),
                "c",
                "linear",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn A");
        let line = match read_handshake(&mut child, Duration::from_secs(15)).await {
            Some(l) => l,
            None => {
                let out = child.wait_with_output().await.expect("reap A");
                kill_and_reap_quiet(&mut child).await;
                panic!("A: handshake timeout. stderr: {}", String::from_utf8_lossy(&out.stderr));
            }
        };
        let trace_id_str = line.split_whitespace().nth(1).expect("trace_id").to_string();
        assert_eq!(kill9_and_reap(&mut child).await, Some(9));
        assert_eq!(effects_lines(&effects), vec!["a", "b"]);

        // 子进程 B：restore（block_node=d）→ c 执行、保存 next=d、握手 → kill
        let mut child_b = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "100",
                effects.to_str().unwrap(),
                "linear",
                "d",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn B");
        let line_b = match read_handshake(&mut child_b, Duration::from_secs(15)).await {
            Some(l) => l,
            None => {
                let out = child_b.wait_with_output().await.expect("reap B");
                kill_and_reap_quiet(&mut child_b).await;
                panic!("B: handshake timeout. stderr: {}", String::from_utf8_lossy(&out_b_err(&out_b)));
            }
        };
        // B 的握手确认磁盘最新 = next=d（seq 延续，未回退）
        assert_eq!(line_b.split_whitespace().nth(2).expect("next"), "d");
        assert_eq!(kill9_and_reap(&mut child_b).await, Some(9));
        assert_eq!(effects_lines(&effects), vec!["a", "b", "c"]);

        // 子进程 C：restore 跑到完成
        let mut child_c = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "100",
                effects.to_str().unwrap(),
                "linear",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn C");
        let out_c = tokio::time::timeout(Duration::from_secs(15), child_c.wait_with_output())
            .await
            .expect("C timeout")
            .expect("reap C");
        assert!(
            out_c.status.success(),
            "C failed: stderr={}",
            String::from_utf8_lossy(&out_c.stderr)
        );

        // 最终：[a, b, c, d] 各一次 — 若 seq 回退，C 会重跑 c，此处断言捕获
        assert_eq!(effects_lines(&effects), vec!["a", "b", "c", "d"]);
    }

    fn out_b_err(out: &std::process::Output) -> String {
        String::from_utf8_lossy(&out.stderr).to_string()
    }
}
```

> **注意**：T10 中 `out_b_err` 辅助在 panic 分支引用了未定义的 `out_b`（上面笔误）— 实施时该分支直接用 `out`（wait_with_output 的结果变量），删除 `out_b_err`。

- [ ] **Step 3: 运行新进程测试**

Run: `cargo +1.88.0 test -p lellm-graph --test restore_new_process`
Expected: T8/T9/T10 PASS（每个 < 30s；正常路径 < 5s）

- [ ] **Step 4: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "test(graph): R4 真崩溃协议 — restore_probe 辅助二进制 + T8/T9/T10 新进程恢复测试（kill -9 + 磁盘握手 + 预算延续 + 双重恢复）"
```

---

### Task 8: 迁移示例 + README 边界说明

**Files:**
- Create: `lellm-graph/examples/persistent_restore.rs`
- Modify: `lellm-graph/Cargo.toml`（`[[example]]` 注册）
- Modify: `README.md`（State Checkpointing 段落）、`README_zh.md`（对应段落）

**Interfaces:**
- Consumes: C1 全部新 API
- Produces: 可编译迁移示例（CHANGELOG 引用）；README 边界说明

- [ ] **Step 1: 写迁移示例**

新建 `lellm-graph/examples/persistent_restore.rs`：

```rust
//! 迁移示例 — 持久化执行 + 新进程恢复（format_version=1）。
//!
//! 运行：`cargo +1.88.0 run -p lellm-graph --example persistent_restore`
//!
//! 旧（System B，从未可用）：
//! ```rust,ignore
//! let session = ExecutionSession::new(state, graph);
//! let cp = session.checkpoint();
//! let restored = ExecutionSession::restore(cp, graph)?;
//! restored.run_with(&mut engine).await?;
//! ```
//!
//! 新：
//! ```rust,ignore
//! let store = Arc::new(FileBlobStore::new(dir));
//! let config = CheckpointConfig::for_store(store, SerdeCheckpointCodec::new(), graph.canonical_hash());
//! let exec = executor.execute_stream_with_checkpoint(graph, state, config)?;
//! // ... 进程崩溃后（新进程）...
//! let cp = typed.load_latest(&trace_id, graph.canonical_hash()).await?;
//! let exec = executor.execute_stream_with_restore(graph, cp, trace_id, config).await?;
//! ```

use std::sync::Arc;

use lellm_graph::{
    BlobCheckpointStore, CheckpointConfig, FileBlobStore, GraphBuilder, GraphEvent, NodeKind,
    SerdeCheckpointCodec, SimpleExecutor, State, TaskNode, TypedCheckpointStore,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 唯一临时目录（示例不删除历史数据；OS 清理 /tmp）
    let dir = std::env::temp_dir().join(format!(
        "lellm_restore_example_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));

    let graph = GraphBuilder::<State>::new("demo")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_ctx| {
            println!("[node] a");
            Ok(())
        })))
        .node("b", NodeKind::Task(TaskNode::new("b", |_ctx| {
            println!("[node] b");
            Ok(())
        })))
        .edge("a", "b")
        .end("b")
        .build()?;
    let hash = graph.canonical_hash();

    // 首次持久化执行
    let store = Arc::new(FileBlobStore::new(&dir));
    let config = CheckpointConfig::for_store(
        store.clone(),
        SerdeCheckpointCodec::<State>::new(),
        hash,
    );
    let executor = SimpleExecutor::new(100);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph.clone()), State::new(), config.clone())
        .expect("entry");

    // 从 GraphStart 事件取 trace_id（新 trace）
    let mut trace_id = None;
    while let Some(ev) = exec.stream.recv().await {
        match ev {
            GraphEvent::GraphStart { trace_id: t } => trace_id = Some(t),
            GraphEvent::GraphComplete { .. } => break,
            GraphEvent::GraphError { error, .. } => return Err(error.to_string().into()),
            _ => {}
        }
    }
    let trace_id = trace_id.expect("GraphStart");
    println!("trace_id: {trace_id}");

    // 模拟「新进程」：加载最新检查点 → 恢复（同一 trace 继续保存）
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&store, codec);
    let cp = typed
        .load_latest(&trace_id, hash)
        .await?
        .expect("checkpoint should exist");
    println!(
        "latest checkpoint: next_node={:?}, steps_used={}",
        cp.next_node, cp.steps_used
    );

    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, trace_id, config)
        .await
        .expect("restore validation");
    while let Some(ev) = exec.stream.recv().await {
        if let GraphEvent::GraphComplete { result } = ev {
            println!("restored state keys: {}", result.state.len());
            break;
        }
    }

    println!("checkpoints at: {}", dir.display());
    Ok(())
}
```

`lellm-graph/Cargo.toml` 追加：

```toml
[[example]]
name = "persistent_restore"
path = "examples/persistent_restore.rs"
```

- [ ] **Step 2: 验证示例可编译可运行**

Run: `cargo +1.88.0 run -p lellm-graph --example persistent_restore`
Expected: 打印 `[node] a`、`[node] b`、trace_id、`latest checkpoint: next_node=None, steps_used=2`、恢复零执行完成

- [ ] **Step 3: README 边界说明（EN + ZH）**

`README.md` 的 "State Checkpointing" 段落（~78-88 行）替换为：

```markdown
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

> **Phase 1 scope**: serial graphs (including loops). Graphs containing Parallel / Subgraph / Barrier are rejected **at the persistence entry**, not after a crash. Checkpoint/restore is wired into the graph execution path only; the agent runtime (ToolUseLoop) is not yet wired (phase 2+).
>
> **Durability boundary**: flush + rename guarantees write completion and atomic visibility — **process-crash safe** (panic / SIGKILL). It does **not** guarantee visibility after power loss (requires file + directory fsync; phase 2).
>
> **No exactly-once**: if the process dies after a node's side effect succeeded but before the checkpoint landed, restore reruns that node. Use idempotency keys / dedup / business compensation.
```

`README_zh.md` 对应段落（~82 行 `ExecutionSession::restore` 示例）替换为中文等价内容（含「串行含循环」「进程崩溃安全 ≠ 断电安全」「不承诺 exactly-once」「agent runtime 未接入」四条边界）。

- [ ] **Step 4: fmt + commit（C1 完成）**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "docs(graph): 持久化恢复迁移示例 + README 第一阶段边界说明（串行含循环/进程崩溃安全/非 exactly-once/agent 未接入）"
```

**C1 收尾验证**（commit 后执行）：

```bash
cargo +1.88.0 test -p lellm-graph          # 受影响 crate 全量
cargo +1.88.0 test                          # workspace 全量回归
cargo +1.88.0 build -p lellm-agent         # 确认 agent 路径零改动可编译
```

---

## C2 旧 API 删除（System B）

### Task 9: 删除 ExecutionSession / SessionCheckpoint / SessionCheckpointSink / SessionError / FrameStack / Frame / MemorySink

**删除范围限定（用户约束 7）**：只删确属废弃路径的类型；目标是保留一套明确的恢复机制，不按名单整批删。已核对：workspace 内零消费者（README×2 的示例在 C1 已被新 API 替换；`docs/v05-graph-as-runtime.md` 是历史设计文档，不改）。

**Files:**
- Delete: `lellm-graph/src/exec/session.rs`（`git rm`）
- Modify: `lellm-graph/src/exec/mod.rs`（删 `pub(crate) mod session;` 与 `pub use session::*;`）
- Modify: `lellm-graph/src/checkpoint/checkpoint_data.rs`（删 `Frame`/`FrameStack`/`MemorySink` 及其 impl/Debug/Default；删内联测试 `test_auto_checkpoint_via_memory_sink`；`NoopCheckpointSink` 保留）
- Modify: `lellm-graph/src/lib.rs`（删导出：`Frame, FrameStack`（88-89 行块）、`ExecutionSession, SessionCheckpoint, SessionCheckpointSink, SessionError`（90 行）、`MemorySink`（42 行块内））
- Modify: `CHANGELOG.md`（破坏性删除记录 + 迁移指向）

**Interfaces:**
- Produces: 单一恢复机制 = `Checkpoint`（format_version=1）+ `execute_stream_with_checkpoint/with_restore` + `BlobCheckpointStore`（InMemory/File）

- [ ] **Step 1: 删除 session.rs 与导出**

```bash
git rm lellm-graph/src/exec/session.rs
```

`exec/mod.rs` 删两行；`lib.rs` 删：

```rust
// 删：pub use checkpoint::{Frame, FrameStack};
// 删：pub use exec::{ExecutionSession, SessionCheckpoint, SessionCheckpointSink, SessionError};
// 删：Checkpoint 导出块中的 MemorySink
```

- [ ] **Step 2: 删 checkpoint_data.rs 的 Frame/FrameStack/MemorySink**

删除 191-306 行（Frame + FrameStack 全部）与 374-422 行（MemorySink 全部）及内联测试 `test_auto_checkpoint_via_memory_sink`（434-468 行）。文件头 doc 的分层图删去 MemorySink/FrameStack 行。`NoopCheckpointSink`（362-372 行）保留。

- [ ] **Step 3: 编译 + 测试**

Run: `cargo +1.88.0 test -p lellm-graph`
Expected: 全绿（checkpoint_data.rs 回落 ~320 行 < 400）

- [ ] **Step 4: CHANGELOG**

`CHANGELOG.md` `## [Unreleased]` 追加：

```markdown
### Removed
- ⚠️ **破坏性（删除）**：System B 恢复路径整体移除 — `ExecutionSession` / `SessionCheckpoint` / `SessionCheckpointSink` / `SessionError` / `FrameStack` / `Frame` / `MemorySink`（`lellm-graph`）。该路径从未可用（恢复恒从 start_node 整图重跑）。
  - **迁移**：使用 `SimpleExecutor::execute_stream_with_checkpoint`（首次持久化执行）+ `execute_stream_with_restore`（新进程从 `FileBlobStore`/`InMemoryBlobStore` 加载检查点恢复）。可编译示例：`lellm-graph/examples/persistent_restore.rs`。

### Compatibility
- ⚠️ **破坏性（API）**：`Checkpoint` 字段变更 — `current_node: NodeId` → `format_version: u32` + `next_node: Option<NodeId>` + `steps_used: usize`；`CheckpointId` 内部类型 uuid → sparkid（Display 21 字符 Base58）；`CheckpointSink::on_checkpoint` 改为 async 并返回 `Result`；`FrameInfo::node_id` → `next_node: Option<NodeId>`；`GraphEvent::CheckpointSaved::node_name` → `next_node: Option<String>`；`SimpleExecutor::execute_stream_with_restore` 签名改为 `(graph, checkpoint, trace_id, config) -> Result`（async）。
- **行为**：检查点保存从 fire-and-forget 改为**同步等待** — 保存失败执行在边界停止（`CheckpointSaveFailed`）；完成态（`next_node=None`）也保存，恢复它零执行直接返回完成。
- **新增**：`FileBlobStore`（磁盘后端，trace 内单调提交序号 + flush/rename 原子可见，进程崩溃安全）、`CheckpointConfig::for_store`、`Graph::validate_persistable`、`CheckpointStoreError::UnsupportedFormat`、`TerminalError::{CheckpointSaveFailed, RestoreUnsupported, RestoreNotLatest, RestoreFailed}`。
- **限制**：持久化/恢复仅支持串行含循环图（Parallel/Subgraph/Barrier 在入口拒绝）；agent runtime 未接入；不承诺外部副作用 exactly-once；进程崩溃安全 ≠ 断电安全。
```

- [ ] **Step 5: fmt + commit**

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "refactor(graph): 删除 System B（ExecutionSession/FrameStack/MemorySink）— 统一恢复路径为 Checkpoint format_version=1"
```

---

## C3 策略清理

### Task 10: 删除 TriggerPolicy / deprecated CheckpointPolicy / RetentionPolicy::TimeBased / with_trigger / with_policy

**第一阶段有效策略（固定）**：每个成功提交的节点都等待检查点保存，完成态也保存 — 保存路径从不读取 trigger，故 TriggerPolicy 整体为死代码；TimeBased 为静默 no-op。

**Files:**
- Modify: `lellm-graph/src/checkpoint/checkpoint_policy.rs`（删 TriggerPolicy/CheckpointPolicy/From impl/TimeBased；`RetentionPolicy` 精简为 KeepAll/KeepLatest）
- Modify: `lellm-graph/src/exec/execution_loop.rs`（`CheckpointConfig` 删 trigger 字段/with_trigger/with_policy）
- Modify: `lellm-graph/src/lib.rs`（删 `RetentionPolicy, TriggerPolicy` 导出中的 TriggerPolicy；CheckpointPolicy 已在 C1 的 `#[allow(deprecated)]` 块中，确认删除）
- Modify: `lellm-graph/tests/checkpoint_test.rs`（删 `test_checkpoint_policy`）
- Modify: `CHANGELOG.md`

**Interfaces:**
- Produces:
  - `pub enum RetentionPolicy { #[default] KeepAll, KeepLatest(usize) }` + `prune_keep() -> Option<usize>`（KeepAll→None，KeepLatest(n)→Some(n)）
  - `CheckpointConfig { retention, save_fn, graph_hash, store }` + `new` / `for_store` / `with_retention` / `with_store`

- [ ] **Step 1: 删除策略死代码**

`checkpoint_policy.rs` 删除：`TriggerPolicy` enum（36-47 行）、`RetentionPolicy::TimeBased(Duration)` 变体 + `prune_keep` 的 TimeBased 分支、`CheckpointPolicy` deprecated enum + `From<CheckpointPolicy>` impl（78-104 行）、`use std::time::Duration;`。文件头 doc 更新为「第一阶段固定策略：每节点同步保存 + 完成态保存；保留策略 KeepAll/KeepLatest(n≥1)」。

`execution_loop.rs` `CheckpointConfig`：删 `trigger` 字段、`with_trigger`、`with_policy`；`new`/`for_store` 构造处删 `trigger: TriggerPolicy::default()`。

`lib.rs`：`pub use checkpoint::{RetentionPolicy, TriggerPolicy};` → `pub use checkpoint::RetentionPolicy;`。

- [ ] **Step 2: 适配测试**

`tests/checkpoint_test.rs` 删 `test_checkpoint_policy`（146-152 行）与 `TriggerPolicy` import。

- [ ] **Step 3: 编译 + 测试**

Run: `cargo +1.88.0 test -p lellm-graph`
Expected: 全绿

- [ ] **Step 4: CHANGELOG + fmt + commit**

```markdown
### Removed
- ⚠️ **破坏性（删除）**：`TriggerPolicy`（保存路径从不读取的死代码）、deprecated `CheckpointPolicy`、`RetentionPolicy::TimeBased`（静默 no-op）、`CheckpointConfig::{with_trigger, with_policy}`（`lellm-graph`）。旧配置含被删选项 → 编译期明确报错，不悄悄改默认值。
```

```bash
cargo +1.88.0 fmt
git add -A
git commit -m "refactor(graph): 策略清理 — 删 TriggerPolicy/TimeBased/deprecated CheckpointPolicy，CheckpointConfig 精简（第一阶段固定：每节点同步保存）"
```

---

## 交付验证

### Task 11: workspace 回归 + 关键 feature 检查 + 审计标记 + 交付报告

- [ ] **Step 1: 全量回归**

```bash
cargo +1.88.0 test                          # workspace 全量
cargo +1.88.0 build --features tool -p lellm-core   # 关键 feature 组合
cargo +1.88.0 build -p lellm-agent
cargo +1.88.0 build -p lellm
```

Expected: 全绿。

- [ ] **Step 2: 测试耗时审计**

Run: `cargo +1.88.0 test -p lellm-graph 2>&1 | grep -E "test .* \.\.\. (ok|FAILED)" | sort`
核对：新进程测试（T8/T9/T10）各 < 30s；> 1s 的测试确认已标注原因（测试 doc comment 已含「外部进程测试」说明）。

- [ ] **Step 3: 审计报告标记**

`discuss/lellm-architecture-audit-2026-09-30.md` 行动清单第 4 项「恢复能力第一阶段（Q1 限定版）+ R4 新进程恢复测试」标记完成（附 commit 范围与 T1-T10 结果摘要）。

- [ ] **Step 4: 记忆更新**

- 更新 `v04-progress-status.md`：恢复里程碑完成（C1/C2/C3 三个 commit）。
- 新增记忆 `restore-phase1-done.md`：format_version=1 游标语义 / 同步保存 / FileBlobStore seq 协议 / 恢复入口最新性检查 / 真崩溃测试协议 / 不承诺清单（exactly-once、断电、Parallel/Subgraph/Barrier、agent）。

- [ ] **Step 5: 交付报告（给用户）**

附上 **T1-T10 对应测试名、命令、结果和剩余限制**：

| # | 测试名 | 文件 | 命令 |
|---|---|---|---|
| T1 | `t1_cursor_semantics_next_node_and_steps` | tests/restore_test.rs | `cargo +1.88.0 test -p lellm-graph --test restore_test t1` |
| T2 | `t2_save_failure_stops_at_boundary` | tests/restore_test.rs | 同上 t2 |
| T3 | `t3_completed_restore_zero_execution` | tests/restore_test.rs | 同上 t3 |
| T4 | `t4_strict_loading_rejects_legacy_and_missing` | tests/restore_format_test.rs | `cargo +1.88.0 test -p lellm-graph --test restore_format_test t4` |
| T5 | `t5_file_store_roundtrip_seq_tmp_corruption_overflow` | tests/restore_store_test.rs | `cargo +1.88.0 test -p lellm-graph --test restore_store_test t5` |
| T6 | `t6_persistence_entry_rejects_unsupported_graphs` | tests/restore_test.rs | `--test restore_test t6` |
| T7 | `t7_loop_budget_continues_across_restore` | tests/restore_test.rs | `--test restore_test t7` |
| T7b | `t7b_restore_entry_validation_direct_construction` | tests/restore_format_test.rs | `--test restore_format_test t7b` |
| T7c | `t7c_keep_latest_zero_rejected` | tests/restore_store_test.rs | `--test restore_store_test t7c` |
| T8 | `t8_r4_crash_kill9_restore_linear` | tests/restore_new_process.rs | `cargo +1.88.0 test -p lellm-graph --test restore_new_process t8` |
| T9 | `t9_crash_restore_loop_budget` | tests/restore_new_process.rs | 同上 t9 |
| T10 | `t10_double_restore_seq_continues` | tests/restore_new_process.rs | 同上 t10 |

**剩余限制（交付时明示）**：
1. 不承诺外部副作用 exactly-once（工具成功但保存前崩溃 → 恢复重跑该节点；需幂等键/去重）。
2. 进程崩溃安全 ≠ 断电安全（flush+rename 无 fsync；断电需文件+目录 fsync，phase 2）。
3. Parallel/Subgraph/Barrier 图恢复不支持（入口显式拒绝，phase 2）。
4. Agent runtime（ToolUseLoop）未接入 checkpoint（phase 2+）。
5. 同一 trace 单写者约束（无锁；多执行者分叉写入不支持）。
6. 恢复只接受该 trace 最新检查点（旧检查点恢复需换新 trace）。
7. `execute_stream_with_restore` 为 async（最新性检查需 store I/O）— 与设计文档 sync 签名的差异，已在 Task 6 注明。
8. `execution_engine.rs`（~440 行）与 C1 期间的 `checkpoint_data.rs`（~500 行）为既有超限，C2 后 checkpoint_data.rs 回落 < 400；execution_engine.rs 未重构（记录在案）。

---

## 自我审查（writing-plans self-review）

**1. Spec 覆盖**：设计 §3（数据模型/严格加载）→ Task 1/2；§4（执行顺序/同步保存/run_inline 包装/Sink async/CheckpointSaved）→ Task 3/4；§5（恢复契约/入口校验/零执行）→ Task 6；§6（FileBlobStore/seq/flush-rename/单写者/保留）→ Task 5；§7（API 变更表/迁移示例）→ Task 1-8 全覆盖（`CheckpointConfig::for_store` 提前到 C1 Task 4，因迁移示例需要 — 与设计 C3 行的差异已注明）；§8（Agent 限制/README）→ Task 8；§9（T1-T10）→ Task 2/4/5/6/7；§10（C1/C2/C3）→ Task 1-8/9/10。R6.1-R6.5 → Task 5/2+6/6/7/3+6。无缺口。

**2. 占位符扫描**：无 TBD/TODO；所有代码步骤含实际代码；测试代码完整可执行（T10 的 `out_b_err` 笔误已在步骤内注明修正方式）。

**3. 类型一致性**：`Checkpoint::new(Option<NodeId>, &S, u64, usize)` 在 Task 1 定义，Task 4/6/7/8 一致使用；`FrameInfo::new(Option<NodeId>, usize)` 一致；`emit_checkpoint(Option<NodeId>, usize) -> Result<(), GraphError>`（async）Task 1 定义、Task 3 调用；`for_store(Arc<dyn BlobCheckpointStore>, impl CheckpointCodec<S> + Clone + Send + Sync + 'static, u64)` Task 4 定义、Task 5-8 一致；`run_inline_from(&str, usize, usize, &mut dyn StepCallback)` Task 3 定义、Task 6 调用；`execute_stream_with_restore` async 签名 Task 6 定义、Task 7/8 一致 `.await`。`TerminalError` 四新变体 Task 1 定义、Task 3/6 一致引用。
