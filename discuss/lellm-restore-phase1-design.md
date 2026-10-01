# LeLLM 恢复能力第一阶段（Q1 限定版）设计

- **日期**：2026-10-01
- **性质**：讨论/评审文档（`discuss/`），非正式交付文档
- **来源**：架构审计报告（`discuss/lellm-architecture-audit-2026-09-30.md`）§5 Q1 + R4
- **状态**：设计已确认（4 项决策 + 用户 5 条约束 + 3 条边界限定），待实施

---

## 1. 背景与目标

审计确认：README 宣传的「Durable Execution」实际是「还原 state + 整图重跑 + best-effort 内存快照」。具体缺陷：

| # | 缺陷 | 位置 |
|---|---|---|
| 1 | 游标语义矛盾：`Checkpoint.current_node` 注释写「下一个要执行的节点」，实际存**刚完成节点** | `checkpoint_data.rs:96-97` |
| 2 | 两套 checkpoint 对象（`Checkpoint` vs `SessionCheckpoint`），恢复契约不统一 | `checkpoint_data.rs` / `exec/session.rs` |
| 3 | 保存是 `tokio::spawn` fire-and-forget，失败只 `tracing::warn` | `execution_loop.rs:144` |
| 4 | 恢复只还原 state，`current_node` 无消费者，恒从 `start_node` 整图重跑 | `execution_loop.rs:284` + `graph_core.rs:346` |
| 5 | 仅内存 store（`InMemoryBlobStore`），无磁盘后端 | `store.rs` |
| 6 | Agent 主路径 `checkpoint=None`，未接入 | `runtime.rs:136/222` |
| 7 | 15 个 checkpoint 测试全是内存对象存取，零恢复链路覆盖 | `tests/` |

**第一阶段目标**：让「保存 → 进程终止 → 新进程从检查点续跑」在**明确限定的范围内**真实工作，且每个边界都有测试钉死。

**明确不承诺**（写进文档与测试注释）：

- **exactly-once 工具执行**：工具成功但检查点未落盘时崩溃，恢复会重跑该节点。需幂等键/结果去重/业务补偿，暂缓。
- **断电 / OS 崩溃持久性**：temp+rename 只保证原子替换（无部分文件），不保证掉电后数据可见。本阶段承诺 = **进程崩溃恢复**（panic / kill -9）。承诺断电需文件 + 目录 fsync，不在本阶段。
- **Parallel / Subgraph / Barrier 图恢复**：显式拒绝，phase 2。
- **Agent runtime checkpoint**：不接入，文档 + API 层面明确限制。

---

## 2. 已确认决策

| # | 决策 | 结论 |
|---|---|---|
| D1 | 游标语义 | 存「**下一个要执行的节点**」：`next_node: Option<NodeId>`，`None` = 已完成。恢复直接使用已确定的执行位置，不重新解析上一节点路由 |
| D2 | 支持范围 | **串行图（含循环）**：Task / Condition / External / ExternalLeaf。Parallel / Subgraph / Barrier 在**启动持久化执行时显式拒绝**（不等崩溃后恢复才报错），恢复入口再校验一次；**非持久化执行保持原行为** |
| D3 | System B | 删除（`SessionCheckpoint` / `SessionCheckpointSink` / `ExecutionSession` / `SessionError` / `FrameStack` / `Frame`），删除前核对公开导出/README/examples，CHANGELOG 记录破坏性删除 + 迁移示例 |
| D4 | 策略层 | 第一阶段有效策略固定为：**每个成功提交的节点都等待检查点保存，完成态也保存**。删除 `TriggerPolicy`（整个 enum，保存路径从不读取）与 `RetentionPolicy::TimeBased`（静默 no-op）；保留 `KeepAll` / `KeepLatest` |

**用户约束（逐条落实）：**

1. **提交顺序固定**：节点执行成功 → 提交状态 → 确定下一节点或完成 → **同步 await 保存检查点** → 执行下一节点。保存失败立即返回错误，不越过边界。
2. **完成态也保存**：结束节点执行完保存 `next_node=None` 的检查点；恢复它 → 直接返回完成态 state，**零节点执行**。首次运行（从 start）与「恢复已完成」是两个明确区分的入口。
3. **校验前移**：含 Parallel/Subgraph/Barrier 的图在启动持久化执行时就拒绝；恢复入口再校验一次；非持久化执行不受影响。
4. **格式版本**：`Checkpoint` 加 `format_version`；旧 `current_node` 语义明确拒绝；**禁止**「缺 `next_node` 被 serde 默认成 `None` 误判为已完成」。
5. **R4 承诺限定**：只验证「检查点成功保存后终止进程 → 新进程恢复不重跑已提交节点」；不证明「工具成功但保存前崩溃」不重复执行。另测：保存失败阻止后续节点、完成态恢复零执行。
6. **循环步数预算**：检查点保存**已执行步数**（`steps_used`），恢复时预算延续，不重置 `max_steps`。补「循环中途保存 → 新进程恢复」测试。
7. **删除范围限定**：System B 只删确属废弃路径的类型；目标是保留一套明确的恢复机制，不按名单整批删。旧配置含被删选项 → 明确报错（编译期），不悄悄改默认值。
8. **提交拆分**：恢复实现 / 旧 API 删除 / 策略清理，三个独立 commit，便于检查和回退。

---

## 3. 数据模型：Checkpoint v2

```rust
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct Checkpoint<S: WorkflowState = State> {
    /// 格式版本 — 反序列化时严格校验（缺失或不支持 → 拒绝）
    pub format_version: u32,
    /// 唯一标识
    pub checkpoint_id: CheckpointId,
    /// 下一个要执行的节点；None = 执行已完成
    pub next_node: Option<NodeId>,
    /// 物化状态快照（S::Checkpoint 投影）
    pub state: S::Checkpoint,
    /// 图结构指纹 — 恢复时校验兼容性
    pub graph_hash: u64,
    /// 已执行步数 — 恢复时步数预算从此延续（不重置 max_steps）
    pub steps_used: usize,
    /// 创建时间
    pub created_at: std::time::SystemTime,
}
```

### 3.1 严格反序列化

`serde` 对缺失的 `Option` 字段**不保证**报错（derive 行为依赖字段默认值规则），必须显式验证：

```rust
// 影子结构：双层 Option 区分「字段缺失」与「显式 null」
#[derive(Deserialize)]
struct RawCheckpoint<S> {
    format_version: u32,                    // 缺失 → serde 报错（非 Option、无 default）
    checkpoint_id: CheckpointId,
    next_node: Option<Option<NodeId>>,      // 外层 = 字段是否存在；内层 = null 与否
    state: S::Checkpoint,
    graph_hash: u64,
    steps_used: usize,
    created_at: std::time::SystemTime,
}
```

判定规则（自定义 `Deserialize for Checkpoint`）：

| 输入 | 结果 |
|---|---|
| 字段缺失 `next_node` | `Err(UnsupportedFormat)` — **不得**解释为已完成 |
| 显式 `"next_node": null` | `Ok(next_node = None)` — 已完成 |
| `"next_node": "node_c"` | `Ok(next_node = Some("node_c"))` |
| 字段缺失 `format_version` | `Err(UnsupportedFormat)` |
| `format_version != 1` | `Err(UnsupportedFormat)` |
| JSON 语法损坏 | `Err(Corrupted)`（与格式不兼容区分开） |
| 旧格式（有 `current_node`、无 `format_version`） | `Err(UnsupportedFormat)` |

新增 `CheckpointStoreError::UnsupportedFormat(String)` 变体。

### 3.2 FrameInfo v2（CheckpointSink 输入）

```rust
pub struct FrameInfo {
    /// 下一个要执行的节点（None = 已完成）
    pub next_node: Option<NodeId>,
    /// 执行步数（从运行入口累计；恢复时从 steps_used 延续）
    pub step: usize,
}
```

---

## 4. 执行顺序与同步保存

### 4.1 run_inline 新循环（唯一执行路径）

```text
loop {
    step += 1
    if step > max_steps → Err(StepsExceeded)
    execute(node)
    commit()
    take_control() → (next_action, signal)
    [Barrier Pause → wait → outcome → 路由]      # 非持久化图仍支持
    解析 next: Option<NodeId>                     # End/到达 end 节点 → None；Goto/Next → Some
    emit_checkpoint(next, step).await?            # ★ 同步保存，失败 → Err，不越过边界
    match next { None → return Ok, Some(n) → current = n }
}
```

对比现状（`execute → commit → checkpoint(刚完成节点) → take_control → route`）：发射点移到**路由解析之后**，保存的 `next_node` 字面成立。

### 4.2 run_inline 签名

```rust
pub async fn run_inline<'cb>(
    &self,
    exec_ctx: &mut ExecutionEngine<'_, S>,
    start_node: &str,      // 首次运行传 start_node()；恢复传 checkpoint.next_node
    steps_used: usize,     // 首次运行 0；恢复传 checkpoint.steps_used
    max_steps: usize,
    step_cb: &mut dyn StepCallback<'cb>,
) -> Result<(), GraphError>
```

调用方适配（全 workspace 内）：`execution_loop.rs`、`subgraph_spec.rs`（子图内层，传 start+0）、`lellm-agent/runtime.rs`（2 处，传 start+0）、3 个 examples、内部测试。

### 4.3 CheckpointSink 改异步

```rust
// 与 BlobCheckpointStore 一致，使用 async_trait（仓库既有模式）
#[async_trait]
pub trait CheckpointSink<S: WorkflowState>: Send + Sync {
    async fn on_checkpoint(
        &mut self, state: &S, frame: &FrameInfo,
    ) -> Result<(), CheckpointStoreError>;
}
```

`ExecutionEngine::emit_checkpoint` 改 async，`CheckpointStoreError` 映射为 `GraphError::Terminal(TerminalError::CheckpointSaveFailed { error })`。

### 4.4 CheckpointSaveSink（去 fire-and-forget）

```rust
async fn on_checkpoint(&mut self, state, frame) -> Result<(), CheckpointStoreError> {
    let cp = Checkpoint::new(frame.next_node.clone(), state, self.graph_hash, frame.step);
    self.save_fn(cp, self.trace_id).await?;          // ★ 同步等待，失败即返回 Err
    // retention prune（best-effort：失败只 warn，不阻断执行 —— 检查点本身已落盘）
    if let Some(keep) = self.retention.prune_keep() {
        if let Some(s) = &self.store { let _ = s.prune(&self.trace_id, keep).await; }
    }
    Ok(())
}
```

### 4.5 CheckpointSaved 事件接线

`GraphEvent::CheckpointSaved` 变体已存在但从未发射（死代码）。本阶段接线：保存成功后 `try_send`，字段 `node_name: String` 改为 `next_node: Option<String>`（与 v2 游标语义一致）。用途：调用方观测保存点（如测试中等待落盘完成再模拟崩溃）。

---

## 5. 恢复契约

### 5.1 入口与校验

```rust
impl SimpleExecutor {
    /// 非持久化（行为不变，不做图结构校验）
    pub fn execute_stream(&self, graph, state) -> GraphExecution<State>;

    /// 持久化：启动执行 + 每节点同步保存检查点
    pub fn execute_stream_with_checkpoint(
        &self, graph: Arc<Graph>, state: State, config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError>;   // 入口校验图结构

    /// 持久化：从检查点恢复（替代旧 execute_stream_with_restore(graph, state, Option<cp>)）
    pub fn execute_stream_with_restore(
        &self, graph: Arc<Graph>, restore_from: Checkpoint<State>,
    ) -> Result<GraphExecution<State>, GraphError>;   // 入口校验图结构 + graph_hash
}
```

校验函数（两个持久化入口共用）：

```rust
fn validate_persistable(graph: &Graph) -> Result<(), GraphError> {
    // 遍历 nodes：Parallel / Subgraph / Barrier → Err(TerminalError::RestoreUnsupported { node, kind })
    // Task / Condition / External / ExternalLeaf → 通过
}
```

- 恢复入口额外校验 `checkpoint.graph_hash == graph.canonical_hash()`（不匹配 → `GraphMismatch`，沿用现有语义）。
- 非持久化 `execute_stream` 不做该校验（原行为）。

### 5.2 恢复流程（run_execution_loop）

```text
restore_from = Some(cp):
    cp.next_node = None  →  零执行：还原 state，发 GraphStart + GraphComplete，直接返回
    cp.next_node = Some(n) →  还原 state，run_inline(start=n, steps_used=cp.steps_used)
restore_from = None:
    run_inline(start=start_node(), steps_used=0)
```

「恢复已完成」与「首次运行」是两个明确区分的分支，前者不执行任何节点。

### 5.3 错误类型（新增）

| 变体 | 触发 |
|---|---|
| `TerminalError::CheckpointSaveFailed { error: String }` | 同步保存失败，执行在该边界停止 |
| `TerminalError::RestoreUnsupported { node: String, kind: String }` | 持久化入口/恢复入口遇到 Parallel/Subgraph/Barrier |
| `CheckpointStoreError::UnsupportedFormat(String)` | 格式版本不符 / `next_node` 字段缺失 / 旧格式 |

---

## 6. FileBlobStore（磁盘后端）

实现 `BlobCheckpointStore` SPI（bytes in / bytes out，与 State 类型和序列化格式解耦）：

```rust
pub struct FileBlobStore { root: PathBuf }
```

**布局**：

```text
<root>/
  <trace_id>/
    <created_at_nanos>_<checkpoint_id_uuid>     # 每个检查点一个文件
    <created_at_nanos>_<checkpoint_id_uuid>.tmp # 写入中的临时文件（崩溃残留，load 时忽略）
```

- **文件内容**：JSON 信封 `{ id, graph_hash, created_at_nanos, data: base64(Vec<u8>) }` —— `data` 是 Codec 产出的字节（默认 SerdeCheckpointCodec = checkpoint JSON），base64 保证对任意 Codec（bincode 等）通用。
- **原子写**：写 `.tmp` → `rename` 到最终名。rename 原子 → 任何时刻目录里只有完整文件（或无关的 `.tmp` 残留）。
- **load_latest**：readdir trace 目录，按 `created_at_nanos` 前缀取最大（同值按 uuid 字典序 tie-break），读文件反序列化。
- **list**：readdir 按 nanos 倒序。**prune(keep)**：保留最新 N 个，`remove_file` 其余。**delete**：`remove_file`。
- **并发**：同一 trace 的检查点由执行循环顺序保存，phase 1 不加锁（文档注明）。
- **落盘边界**：无 fsync → 保证**进程崩溃恢复**（panic / kill -9，page cache 存活），**不保证断电 / OS 崩溃**（需文件 + 目录 fsync，phase 2 评估）。

---

## 7. 公开 API 变更总表

| API | 变更 | 说明 |
|---|---|---|
| `Checkpoint` | **破坏**：`current_node: NodeId` → `format_version` + `next_node: Option<NodeId>` + `steps_used` | 旧格式反序列化拒绝 |
| `CheckpointSink::on_checkpoint` | **破坏**：sync → async，返回 `Result<(), CheckpointStoreError>` | 所有实现方适配 |
| `FrameInfo` | **破坏**：`node_id`（刚完成节点）→ `next_node: Option<NodeId>` | 语义修正 |
| `Graph::run_inline` | **破坏**：新增 `start_node: &str, steps_used: usize` 参数 | 8 处调用方适配 |
| `GraphEvent::CheckpointSaved` | **破坏**：`node_name` → `next_node: Option<String>`；由从不发射改为实际发射 | |
| `SimpleExecutor::execute_stream_with_checkpoint` | **新增** | 持久化执行入口 |
| `CheckpointConfig::for_store(store, codec, graph_hash)` | **新增** | 便捷构造器：从 `Arc<dyn BlobCheckpointStore>` + Codec 生成 save_fn（内部组合，等价于手写 closure） |
| `SimpleExecutor::execute_stream_with_restore` | **破坏**：`(graph, state, Option<cp>)` → `(graph, cp)`，返回 `Result` | 旧签名删除 |
| `SessionCheckpoint` / `SessionCheckpointSink` / `ExecutionSession` / `SessionError` / `FrameStack` / `Frame` / `MemorySink` | **删除** | System B，零消费者（删除前核对 README/examples） |
| `NoopCheckpointSink` | **保留**（适配新签名） | 不依赖 Frame，CheckpointSink 生态的显式空实现 |
| `TriggerPolicy` / `CheckpointPolicy`(deprecated) / `RetentionPolicy::TimeBased` / `CheckpointConfig::with_trigger` / `with_policy` | **删除** | 死代码 / 静默 no-op |
| `FileBlobStore` | **新增** | 磁盘后端 |
| `CheckpointStoreError::UnsupportedFormat` | **新增** | |
| `TerminalError::CheckpointSaveFailed` / `RestoreUnsupported` | **新增** | |
| `src/bin/restore_probe.rs` | **新增** | R4 新进程测试用辅助二进制 |

**迁移示例**（CHANGELOG 记录）：

```rust
// 旧（System B，从未可用）
let session = ExecutionSession::new(state, graph);
let cp = session.checkpoint();
let restored = ExecutionSession::restore(cp, graph)?;
restored.run_with(&mut engine).await?;

// 新
let store = Arc::new(FileBlobStore::new(dir));
let cfg = CheckpointConfig::for_store(store, SerdeCheckpointCodec::new(), graph.canonical_hash());
let exec = executor.execute_stream_with_checkpoint(graph.clone(), state, cfg)?;
// ... 崩溃后 ...
let cp = typed_store.load_latest(&trace_id, graph.canonical_hash()).await?;
let exec = executor.execute_stream_with_restore(graph, cp)?;
```

---

## 8. Agent 路径限制（Q1.5）

- `lellm-agent` runtime 保持 `checkpoint=None`（仅做 `run_inline` 签名机械适配）。
- README（EN/ZH）+ checkpoint API doc comment 明确：**checkpoint/恢复仅接入图执行路径；agent runtime（ToolUseLoop）未接入**，属 phase 2+。
- `AgentState` 的 `AgentCheckpoint` 投影保留（类型基础设施就绪），不新增 agent checkpoint API。

---

## 9. 测试计划

> 工具链 `cargo +1.88.0`（stable 本机损坏）。单测 < 10s，外部调用 < 30s。

### 9.1 单元测试（lellm-graph）

| # | 测试 | 钉死的契约 |
|---|---|---|
| T1 | 游标语义：节点 b 后检查点 `next_node=Some(c)`、`steps_used` 正确；end 节点后 `next_node=None` | D1 + 约束 2 |
| T2 | 保存失败（failing save_fn）→ 执行在该边界停止，后续节点不执行，`GraphError::CheckpointSaveFailed` | 约束 1 / 5b |
| T3 | 完成态恢复：`restore(next=None)` → 零节点执行（副作用计数不变），返回完成态 state | 约束 2 / 5c |
| T4 | 严格反序列化 5 用例：缺 `next_node` → 拒绝；显式 `null` → 已完成；缺 `format_version` → 拒绝；`format_version=999` → 拒绝；旧格式（`current_node`）→ 拒绝 | 约束 4 |
| T5 | FileBlobStore：save/load/load_latest/list/prune 往返；`.tmp` 残留不影响 load_latest | §6 |
| T6 | 图校验：含 Parallel/Subgraph/Barrier 的图 → `execute_stream_with_checkpoint` 入口即 `RestoreUnsupported`；同图 `execute_stream`（非持久化）正常执行 | D2 / 约束 3 |
| T7 | 步数预算延续：循环图 max_steps=5，第 3 步后崩溃（steps_used=3）→ 进程内恢复 → 总第 6 步 `StepsExceeded`（而非重新获得完整预算） | 约束 6 |

### 9.2 集成测试（新进程，R4）

**辅助二进制** `lellm-graph/src/bin/restore_probe.rs`：参数 `<checkpoint_dir> <trace_id> <max_steps>`；从磁盘加载最新检查点 → 重建图 → `execute_stream_with_restore` 续跑 → 打印最终 state JSON。`cargo test` 自动构建，集成测试经 `env!("CARGO_BIN_EXE_restore_probe")` spawn —— **真实新 OS 进程**，内存状态零共享。

| # | 测试 | 场景 | 断言 |
|---|---|---|---|
| T8 | R4 主链路：图 a→b→c→d（每节点向副作用文件追加自身名），FileBlobStore，节点 c 首次执行返回 Err（模拟崩溃） | 父进程：运行至 GraphError，断言磁盘存在 `next=c, steps_used=2` 的检查点，副作用文件 = [a, b]；spawn 子进程恢复（c 本次成功） | 副作用文件 = [a, b, c, d]（a/b 不重跑、c/d 各一次）；最终 state 正确 |
| T9 | 循环中途恢复：循环图 + 小 max_steps，首次运行中途失败 | spawn 子进程恢复 | 执行位置正确 + 步数预算延续（总步数到限才 `StepsExceeded`） |

**T8/T9 承诺边界**（写进测试 doc comment）：验证「检查点**成功保存后**终止进程 → 新进程恢复不重跑已提交节点」。**不能**证明「工具成功但保存前崩溃」不重复执行（那需要幂等键/去重，暂缓项）。

### 9.3 既有测试适配

- `checkpoint_test.rs` / `checkpoint_restore_test.rs`（15 个）：适配 v2 格式（`next_node` / `format_version` / `steps_used`），存/取/hash 链路语义不变。
- `graph_test.rs` / `parallel_test.rs`：`run_inline` 签名适配（如直接调用）。

---

## 10. 提交拆分（3 个独立 commit，便于检查与回退）

| Commit | 内容 | 可编译性 |
|---|---|---|
| **C1 恢复实现** | Checkpoint v2 + 严格反序列化；FrameInfo v2 + CheckpointSink async + emit 后移；`run_inline(start, steps_used)`；CheckpointSaveSink 同步保存 + CheckpointSaved 接线；FileBlobStore；新错误变体；`execute_stream_with_checkpoint` / `with_restore` 新签名；lellm-agent + examples + subgraph_spec 适配；T1-T9 全部测试；README 边界说明 | System B 的两个 Sink 做最小适配（跟随新 FrameInfo/async 签名）保持编译 |
| **C2 旧 API 删除** | 删 `session.rs`（SessionCheckpoint/SessionCheckpointSink/ExecutionSession/SessionError）+ `FrameStack`/`Frame`/`MemorySink`；lib.rs 导出更新；README 迁移示例；CHANGELOG 破坏性删除记录 | C1 之后独立可编译 |
| **C3 策略清理** | 删 `TriggerPolicy` / deprecated `CheckpointPolicy` / `RetentionPolicy::TimeBased` / `with_trigger` / `with_policy`；`CheckpointConfig` 精简；CHANGELOG | C2 之后独立可编译 |

每个 commit 后：`cargo +1.88.0 test -p lellm-graph`（受影响 crate 优先）→ workspace 全量。

---

## 11. 文件清单（预估）

**C1**：`checkpoint/checkpoint_data.rs`（v2 + 严格反序列化）、`checkpoint/store.rs`（FileBlobStore，~300 行 < 400 上限）、`checkpoint/mod.rs`、`exec/execution_loop.rs`、`exec/execution_engine.rs`、`graph/graph_core.rs`、`test_executor.rs`、`error.rs`、`event.rs`、`node/subgraph_spec.rs`、`src/bin/restore_probe.rs`（新）、`tests/restore_new_process.rs`（新）、`tests/checkpoint_test.rs`、`tests/checkpoint_restore_test.rs`、`lellm-agent/src/runtime/runtime.rs`、3 个 examples、README×2

**C2**：`exec/session.rs`（删）、`exec/mod.rs`、`checkpoint/checkpoint_data.rs`、`lib.rs`、README×2、CHANGELOG

**C3**：`checkpoint/checkpoint_policy.rs`、`exec/execution_loop.rs`、`lib.rs`、CHANGELOG

> 文件夹文件数：`checkpoint/` 维持 7 个（FileBlobStore 并入 `store.rs`），不超 8 上限。

---

## 12. 风险与开放点

| 风险 | 缓解 |
|---|---|
| `run_inline` 签名变更影响面（8 处调用方） | 全 workspace 内，C1 一次性适配 + 全量测试 |
| FileBlobStore 无 fsync 被误读为断电安全 | README + 测试注释 + 本设计 §6 三处写明边界 |
| 旧格式检查点（若有人手工构造过）无法加载 | 预期行为：`UnsupportedFormat` 明确报错，不静默降级 |
| `MemorySink` 若有内部测试使用 | 实施时核对；若有，C1 适配、C2 随 Frame 删除并改写测试 |
| prune 失败语义 | 明确为 best-effort（warn），检查点落盘成功即视为越过边界 |
