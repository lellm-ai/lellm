# LeLLM 恢复能力第一阶段（Q1 限定版）设计

- **日期**：2026-10-01
- **性质**：讨论/评审文档（`discuss/`），非正式交付文档
- **来源**：架构审计报告（`discuss/lellm-architecture-audit-2026-09-30.md`）§5 Q1 + R4
- **状态**：设计已确认（4 项决策 + 用户约束 + 5 项评审修正），待实施

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

**第一阶段目标**：让「保存 → 进程异常终止 → 新进程从检查点续跑」在**明确限定的范围内**真实工作，且每个边界都有测试钉死。

**明确不承诺**（写进文档与测试注释）：

- **exactly-once 工具执行**：工具成功但检查点未落盘时崩溃，恢复会重跑该节点。需幂等键/结果去重/业务补偿，暂缓。
- **断电 / OS 崩溃持久性**：flush+rename 保证写入完成与原子可见（进程崩溃安全），不保证掉电后数据可见。本阶段承诺 = **进程崩溃恢复**（panic / SIGKILL）。承诺断电需文件 + 目录 fsync，不在本阶段。
- **Parallel / Subgraph / Barrier 图恢复**：显式拒绝，phase 2。
- **Agent runtime checkpoint**：不接入，文档 + API 层面明确限制。
- **多执行者并发写同一 trace**：同一 trace 同时只能有一个活跃执行者（见 §6.3）。

---

## 2. 已确认决策与约束

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
4. **格式版本**：`Checkpoint` 加 `format_version`；旧 `current_node` 语义明确拒绝；**禁止**「缺 `next_node` 被默认成 `None` 误判为已完成」。
5. **R4 承诺限定**：验证「检查点**成功落盘后进程被强制终止** → 新进程恢复不重跑已提交节点」；不证明「工具成功但保存前崩溃」不重复执行。另测：保存失败阻止后续节点、完成态恢复零执行。
6. **循环步数预算**：检查点保存**已执行步数**（`steps_used`），恢复时预算延续，不重置 `max_steps`（`max_steps` 是**总预算**，恢复时同样传总预算）。
7. **删除范围限定**：System B 只删确属废弃路径的类型；目标是保留一套明确的恢复机制，不按名单整批删。旧配置含被删选项 → 明确报错（编译期），不悄悄改默认值。
8. **提交拆分**：恢复实现 / 旧 API 删除 / 策略清理，三个独立 commit，便于检查和回退。

**评审修正（本轮，逐条落实）：**

| # | 修正 | 落实位置 |
|---|---|---|
| R1 | 双层 `Option` + derive 无法区分缺失/null；`Deserialize` 不能返回 `CheckpointStoreError`。改为 **Codec 两段式**：Value 解析（语法错→Corrupted）→ 结构校验（键缺失/null/版本→UnsupportedFormat）→ 类型化反序列化。不靠错误文本分类 | §3.1 |
| R2 | 恢复入口必须能**继续保存**：传入 `CheckpointConfig` + `trace_id`（延续同一 trace）。补验收：**保存 → 新进程恢复 → 再保存 → 再次新进程恢复** | §5.1 / T10 |
| R3 | `load_latest` 不依赖墙上时钟 + UUID 字典序。改为 **trace 内单调提交序号**（恢复后延续）；时间戳仅展示。`KeepLatest(0)` 拒绝；最新检查点损坏**明确报错、不回退旧检查点**；prune 失败 `tracing::warn` | §6.1 / §6.4 |
| R4 | rename 前必须 `flush().await` 确保写入完成（≠ fsync 断电语义）；单写者约束 = **同一 trace 同时只有一个活跃执行者** | §6.2 / §6.3 |
| R5 | R4 测试改为**真崩溃协议**：子进程运行 → 磁盘确认检查点 + stdout 可靠握手 → 父进程 `kill -9` → 回收 → 新子进程恢复。`CheckpointSaved` 事件（try_send 可能丢）仅尽力观测，不作握手。循环测试验证到预算后下一节点确实未执行 | §9.2 |

**实施小修正：**

- 恢复入口校验覆盖**直接构造**（绕过反序列化）的检查点：`format_version`、图指纹、`next_node` 指向的节点存在、步数边界（`steps_used < max_steps`）。
- `step += 1` 改溢出安全预算检查。
- 命名统一：格式版本就叫 `format_version=1`（首个带版本格式；旧无版本格式称「legacy」，拒绝加载）。不再使用「Checkpoint v2」称呼。
- 迁移示例补全 `typed_store` / `trace_id` / 结果等待方式，做成**可编译 example** 验证。
- 公共 `run_inline` **保留旧签名**，包装新内部入口 `run_inline_from` —— lellm-agent / examples / subgraph_spec 零改动。

---

## 3. 数据模型：Checkpoint（format_version=1）

```rust
pub const CHECKPOINT_FORMAT_VERSION: u32 = 1;

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
```

> 结构体保留普通 derive。**严格性不在 `Deserialize` 实现里**（它只能返回 `D::Error`，无法表达 `UnsupportedFormat`），而在 Codec 加载路径（§3.1）与恢复入口校验（§5.1，覆盖直接构造的检查点）两处。

### 3.1 严格加载：Codec 两段式（不用错误文本分类）

```rust
// SerdeCheckpointCodec::deserialize
fn deserialize(&self, blob, expected_hash) -> Result<Checkpoint<S>, CheckpointStoreError> {
    if blob.graph_hash != expected_hash { return Err(GraphMismatch { .. }); }

    // 段 1：语法 — 解析为通用 Value
    let value: serde_json::Value = serde_json::from_slice(&blob.data)
        .map_err(|e| CheckpointStoreError::Corrupted(e.to_string()))?;
    if !value.is_object() { return Err(CheckpointStoreError::Corrupted("not a JSON object".into())); }

    // 段 2：格式校验 — 对 Value 做结构化检查（三态在此天然可分）
    //   format_version: 键缺失 → UnsupportedFormat；值 != 1 → UnsupportedFormat
    //   next_node:      键缺失 → UnsupportedFormat（禁止解释为已完成）
    //                   Value::Null → 合法（已完成）
    //                   其他 → 合法（类型错误留给段 3 → Corrupted）
    validate_checkpoint_format(&value)?;   // → Err(UnsupportedFormat(..))

    // 段 3：类型化
    serde_json::from_value(value)
        .map_err(|e| CheckpointStoreError::Corrupted(e.to_string()))
}
```

判定表：

| 输入 | 结果 |
|---|---|
| JSON 语法损坏 / 顶层非对象 | `Corrupted` |
| 键缺失 `format_version` | `UnsupportedFormat` |
| `format_version != 1` | `UnsupportedFormat` |
| 键缺失 `next_node` | `UnsupportedFormat` — **不得**解释为已完成 |
| 显式 `"next_node": null` | 合法 → `next_node = None`（已完成） |
| 旧格式（有 `current_node`、无 `format_version`） | `UnsupportedFormat` |
| 段 2 通过但类型不符（如 `next_node` 是数字） | `Corrupted` |

新增 `CheckpointStoreError::UnsupportedFormat(String)` 变体。

### 3.2 FrameInfo（CheckpointSink 输入）

```rust
pub struct FrameInfo {
    /// 下一个要执行的节点（None = 已完成）
    pub next_node: Option<NodeId>,
    /// 已执行步数（从运行入口累计；恢复时从 steps_used 延续）
    pub step: usize,
}
```

---

## 4. 执行顺序与同步保存

### 4.1 run_inline 新循环（唯一执行路径）

```text
loop {
    step = step.checked_add(1).ok_or(overflow)?      # 溢出安全
    if step > max_steps → Err(StepsExceeded)          # max_steps = 总预算（恢复时同样）
    execute(node)
    commit()
    take_control() → (next_action, signal)
    [Barrier Pause → wait → outcome → 路由]           # 非持久化图仍支持
    解析 next: Option<NodeId>                          # End/到达 end 节点 → None；Goto/Next → Some
    emit_checkpoint(next, step).await?                 # ★ 同步保存，失败 → Err，不越过边界
    match next { None → return Ok, Some(n) → current = n }
}
```

对比现状（`execute → commit → checkpoint(刚完成节点) → take_control → route`）：发射点移到**路由解析之后**，保存的 `next_node` 字面成立。

### 4.2 run_inline 签名（保留旧签名，减少破坏面）

```rust
// 公共 API 不变 — 首次运行语义（start_node()，steps_used=0）
pub async fn run_inline<'cb>(&self, exec_ctx, max_steps, step_cb) -> Result<(), GraphError>;

// 新内部入口（pub(crate)）— 恢复与首次运行统一走这里
pub(crate) async fn run_inline_from<'cb>(
    &self,
    exec_ctx: &mut ExecutionEngine<'_, S>,
    start_node: &str,      // 首次运行 = start_node()；恢复 = checkpoint.next_node
    steps_used: usize,     // 首次运行 = 0；恢复 = checkpoint.steps_used
    max_steps: usize,      // 总预算
    step_cb: &mut dyn StepCallback<'cb>,
) -> Result<(), GraphError>;
```

**lellm-agent / examples / subgraph_spec 零改动**（旧签名保留）。仅 `execution_loop.rs` 改调 `run_inline_from`。

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
    // retention prune（best-effort：失败 tracing::warn，不阻断执行 —— 检查点本身已落盘）
    if let Some(keep) = self.retention.prune_keep() {
        if keep == 0 {
            return Err(CheckpointStoreError::Storage("KeepLatest(0) 无效：会删除全部恢复点".into()));
        }
        if let Some(s) = &self.store {
            if let Err(e) = s.prune(&self.trace_id, keep).await {
                tracing::warn!(error = %e, "checkpoint prune failed");
            }
        }
    }
    Ok(())
}
```

### 4.5 CheckpointSaved 事件接线

`GraphEvent::CheckpointSaved` 变体已存在但从未发射（死代码）。本阶段接线：保存成功后 `try_send`，字段 `node_name: String` 改为 `next_node: Option<String>`（与游标语义一致）。**定位：尽力而为的观测信号**（try_send 可能丢）——测试的可靠握手不依赖它（§9.2）。

---

## 5. 恢复契约

### 5.1 入口与校验

```rust
impl SimpleExecutor {
    /// 非持久化（行为不变，不做图结构校验）
    pub fn execute_stream(&self, graph, state) -> GraphExecution<State>;

    /// 持久化：启动执行 + 每节点同步保存检查点（新 trace）
    pub fn execute_stream_with_checkpoint(
        &self, graph: Arc<Graph>, state: State, config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError>;   // 入口校验图结构

    /// 持久化：从检查点恢复，并**继续保存**到同一 trace
    /// - trace_id：延续原 trace（新检查点保存于该 trace 下，load_latest 单调前进）
    /// - config：恢复后继续保存的持久化配置（store/retention）
    pub fn execute_stream_with_restore(
        &self,
        graph: Arc<Graph>,
        restore_from: Checkpoint<State>,
        trace_id: TraceId,
        config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError>;   // 入口校验（下）
}
```

**入口校验**（两个持久化入口共用图结构校验；恢复入口额外校验检查点本身 —— 覆盖绕过反序列化的直接构造）：

| 校验 | 失败错误 |
|---|---|
| 图结构：含 Parallel/Subgraph/Barrier | `TerminalError::RestoreUnsupported { node, kind }` |
| `checkpoint.graph_hash == graph.canonical_hash()` | `GraphMismatch`（沿用现有语义） |
| `checkpoint.format_version == CHECKPOINT_FORMAT_VERSION` | `TerminalError`（格式不支持） |
| `next_node = Some(n)` 时 `n` 存在于 `graph.nodes` | `TerminalError`（节点不存在） |
| `steps_used < max_steps`（步数边界：预算未耗尽） | `TerminalError`（预算已耗尽，无可恢复） |

非持久化 `execute_stream` 不做上述校验（原行为）。

### 5.2 恢复流程（run_execution_loop）

```text
restore_from = Some(cp):
    cp.next_node = None  →  零执行：还原 state，发 GraphStart{trace_id} + GraphComplete，直接返回
    cp.next_node = Some(n) →  还原 state，
        run_inline_from(start=n, steps_used=cp.steps_used) + CheckpointSaveSink(trace_id 延续)
restore_from = None:
    run_inline_from(start=start_node(), steps_used=0) [+ CheckpointSaveSink（持久化入口时）]
```

- 「恢复已完成」与「首次运行」是两个明确区分的分支，前者不执行任何节点。
- 恢复后的新检查点保存于**同一 trace_id**，提交序号延续（§6.1）→ 支持反复「崩溃 → 恢复」循环（T10 验收）。
- 新执行的 `GraphStart { trace_id }` 事件携带延续的 trace_id。

### 5.3 错误类型（新增）

| 变体 | 触发 |
|---|---|
| `TerminalError::CheckpointSaveFailed { error: String }` | 同步保存失败，执行在该边界停止 |
| `TerminalError::RestoreUnsupported { node: String, kind: String }` | 持久化入口/恢复入口遇到 Parallel/Subgraph/Barrier |
| `CheckpointStoreError::UnsupportedFormat(String)` | 格式版本不符 / `next_node` 键缺失 / 旧格式 |

---

## 6. FileBlobStore（磁盘后端）

实现 `BlobCheckpointStore` SPI（bytes in / bytes out，与 State 类型和序列化格式解耦）：

```rust
pub struct FileBlobStore { root: PathBuf }
```

### 6.1 布局与提交序号（不依赖墙上时钟）

```text
<root>/
  <trace_id>/
    <seq>_<checkpoint_id_uuid>     # 每个检查点一个文件
    <seq>_<checkpoint_id_uuid>.tmp # 写入中的临时文件（崩溃残留，load 时忽略）
```

- **`seq` = trace 内单调递增的提交序号**：保存时扫 trace 目录取当前最大 seq + 1（目录即记录，无独立计数器文件，崩溃后自动延续）。`created_at` **仅展示用途，不参与排序**（墙上时钟可相同、可因校时倒退；UUID 字典序不代表提交先后）。
- **load_latest** = seq 最大的文件。**list** 按 seq 倒序。**prune(keep)** 保留 seq 最大的 N 个，`remove_file` 其余。
- **最新检查点损坏 → 明确报错（`Corrupted`），不回退旧检查点**（回退 = 悄悄重跑已提交节点，禁止）。
- `InMemoryBlobStore` 顺序语义 = 插入序（单进程内即提交序，文档注明）；跨进程场景用 `FileBlobStore`。

### 6.2 写入协议（flush 先于 rename）

```text
write_all(信封 JSON).await?
→ flush().await?          # ★ tokio::fs 写返回不代表写入完成，flush 等待完成
→ rename(.tmp → 最终名)    # 同目录 rename 原子 → 目录里任何时刻只有完整文件
→ 报告保存成功
```

- **文件内容**：JSON 信封 `{ id, graph_hash, created_at_nanos, data: base64(Vec<u8>) }` —— `data` 是 Codec 产出的字节（默认 SerdeCheckpointCodec = checkpoint JSON），base64 对任意 Codec（bincode 等）通用。
- **落盘边界**：flush+rename 保证**写入完成 + 原子可见**（进程崩溃安全：panic / SIGKILL 后文件要么完整存在、要么不存在）。**不保证断电 / OS 崩溃**（需文件 + 目录 fsync，phase 2 评估）。两者是不同层次，文档分别写明。

### 6.3 单写者约束

**同一 trace 同时只能有一个活跃执行者**（不是「每个执行循环内部顺序保存」——那是单执行者的内部性质，不约束多执行者）。seq 的「扫目录取 max+1」在并发写者下会冲突（两写者取到同一 seq）。phase 1 不加锁，**文档明示该约束**；多执行者分叉写入不支持。

### 6.4 保留策略

- `KeepAll` / `KeepLatest(n)`（n ≥ 1）有效；**`KeepLatest(0)` → 保存路径明确报错**（避免保存后删掉全部恢复点）。
- prune 失败 → `tracing::warn`（best-effort；检查点落盘成功即视为越过边界）。

---

## 7. 公开 API 变更总表

| API | 变更 | 说明 |
|---|---|---|
| `Checkpoint` | **破坏**：`current_node: NodeId` → `format_version` + `next_node: Option<NodeId>` + `steps_used` | 旧格式（legacy 无版本）加载拒绝 |
| `CheckpointSink::on_checkpoint` | **破坏**：sync → async，返回 `Result<(), CheckpointStoreError>` | 所有实现方适配 |
| `FrameInfo` | **破坏**：`node_id`（刚完成节点）→ `next_node: Option<NodeId>` | 语义修正 |
| `Graph::run_inline` | **不变**（旧签名保留，内部改调 `run_inline_from`） | lellm-agent / examples / subgraph_spec 零改动 |
| `GraphEvent::CheckpointSaved` | **破坏**：`node_name` → `next_node: Option<String>`；由从不发射改为实际发射（尽力观测） | |
| `SimpleExecutor::execute_stream_with_checkpoint` | **新增** | 持久化执行入口（新 trace） |
| `SimpleExecutor::execute_stream_with_restore` | **破坏**：`(graph, state, Option<cp>)` → `(graph, cp, trace_id, config)`，返回 `Result` | 恢复后继续保存到同一 trace |
| `SessionCheckpoint` / `SessionCheckpointSink` / `ExecutionSession` / `SessionError` / `FrameStack` / `Frame` / `MemorySink` | **删除** | System B，零消费者（删除前核对 README/examples） |
| `NoopCheckpointSink` | **保留**（适配新签名） | 不依赖 Frame，CheckpointSink 生态的显式空实现 |
| `TriggerPolicy` / `CheckpointPolicy`(deprecated) / `RetentionPolicy::TimeBased` / `CheckpointConfig::with_trigger` / `with_policy` | **删除** | 死代码 / 静默 no-op |
| `FileBlobStore` | **新增** | 磁盘后端（seq 提交序号 + flush/rename） |
| `CheckpointStoreError::UnsupportedFormat` | **新增** | |
| `TerminalError::CheckpointSaveFailed` / `RestoreUnsupported` | **新增** | |
| `src/bin/restore_probe.rs` | **新增** | R4 新进程测试辅助二进制（run/restore 双模式 + 握手 + 阻塞） |
| `examples/persistent_restore.rs` | **新增** | 迁移示例（可编译验证：typed_store / trace_id / 结果等待） |

**迁移示例**（`examples/persistent_restore.rs`，可编译；CHANGELOG 同步记录）：

```rust
// 旧（System B，从未可用）
let session = ExecutionSession::new(state, graph);
let cp = session.checkpoint();
let restored = ExecutionSession::restore(cp, graph)?;
restored.run_with(&mut engine).await?;

// 新
let store = Arc::new(FileBlobStore::new(dir));
let codec = SerdeCheckpointCodec::<State>::new();
let config = CheckpointConfig::for_store(store.clone(), codec.clone(), graph.canonical_hash());

// 首次持久化执行
let exec = executor.execute_stream_with_checkpoint(graph.clone(), state, config)?;
let trace_id = wait_graph_start(exec.stream).await;   // 从 GraphStart 事件取 trace_id

// ... 进程崩溃后（新进程）...
let typed = TypedCheckpointStore::new(&store, codec);
let cp = typed.load_latest(&trace_id, graph.canonical_hash()).await?
    .expect("checkpoint should exist");
let exec = executor
    .execute_stream_with_restore(graph, cp, trace_id, config)
    .expect("restore validation");
wait_graph_complete(exec.stream).await;   // 消费事件直至 GraphComplete
```

---

## 8. Agent 路径限制（Q1.5）

- `lellm-agent` runtime 保持 `checkpoint=None`（`run_inline` 旧签名保留 → **零改动**）。
- README（EN/ZH）+ checkpoint API doc comment 明确：**checkpoint/恢复仅接入图执行路径；agent runtime（ToolUseLoop）未接入**，属 phase 2+。
- `AgentState` 的 `AgentCheckpoint` 投影保留（类型基础设施就绪），不新增 agent checkpoint API。

---

## 9. 测试计划

> 工具链 `cargo +1.88.0`（stable 本机损坏）。单测 < 10s，外部调用 < 30s。

### 9.1 单元测试（lellm-graph）

| # | 测试 | 钉死的契约 |
|---|---|---|
| T1 | 游标语义：节点 b 后检查点 `next_node=Some(c)`、`steps_used` 正确；end 节点后 `next_node=None`（完成态也保存） | D1 + 约束 2 |
| T2 | 保存失败（failing save_fn）→ 执行在该边界停止，后续节点不执行，`GraphError::CheckpointSaveFailed` | 约束 1 / R5 |
| T3 | 完成态恢复：`restore(next=None)` → 零节点执行（副作用计数不变），返回完成态 state | 约束 2 / R5 |
| T4 | 严格加载 6 用例（Codec 两段式）：JSON 损坏 → Corrupted；缺 `format_version` → UnsupportedFormat；`format_version=999` → UnsupportedFormat；缺 `next_node` 键 → UnsupportedFormat；显式 `"next_node": null` → 已完成；旧格式（`current_node`）→ UnsupportedFormat | 约束 4 / R1 |
| T5 | FileBlobStore：save/load/load_latest/list/prune 往返；seq 单调（含「恢复后延续」：删进程重建 store 再保存，seq 继续）；`.tmp` 残留不影响 load_latest；**最新文件损坏 → load_latest 报 Corrupted（不回退旧检查点）** | §6 |
| T6 | 图校验：含 Parallel/Subgraph/Barrier 的图 → `execute_stream_with_checkpoint` 入口即 `RestoreUnsupported`；同图 `execute_stream`（非持久化）正常执行 | D2 / 约束 3 |
| T7 | 步数预算延续：循环图 max_steps=5，第 3 步后崩溃（steps_used=3）→ 进程内恢复 → 总第 6 步 `StepsExceeded`（而非重新获得完整预算）；**到限后下一节点确实未执行**（副作用计数验证） | 约束 6 / R5 |
| T7b | 恢复入口校验（直接构造的检查点）：`format_version` 错 / `graph_hash` 错 / `next_node` 指向不存在节点 / `steps_used >= max_steps` → 各自明确报错 | 小修正 |
| T7c | `KeepLatest(0)` → 保存路径报错 | §6.4 |

### 9.2 集成测试（新进程，R4 真崩溃协议）

**辅助二进制** `lellm-graph/src/bin/restore_probe.rs`（`cargo test` 自动构建，经 `env!("CARGO_BIN_EXE_restore_probe")` spawn —— 真实新 OS 进程，内存状态零共享）：

- 模式 `run <dir> <effects_file> <block_node>`：持久化执行；到达 `block_node` 时，节点先**直接查 store 确认本检查点已落盘**（磁盘为真，不依赖事件），向 stdout 打印 `HANDSHAKE <trace_id> <next_node>` 并 flush（trace_id 供父进程传递给恢复子进程），然后阻塞等待（模拟长节点，进程在此被杀）。
- 模式 `restore <dir> <trace_id> <max_steps> <effects_file> [block_node]`：加载最新检查点 → `execute_stream_with_restore` 续跑（同一 trace 继续保存）→ 可选阻塞 / 跑到完成打印最终 state JSON。

**可靠握手**：stdout 行（管道，不丢）+ 磁盘确认。**不使用 `CheckpointSaved` 事件作握手**（try_send 可能丢，仅尽力观测）。

| # | 测试 | 协议 | 断言 |
|---|---|---|---|
| T8 | R4 主链路：图 a→b→c→d（每节点向副作用文件追加自身名），FileBlobStore | ① 父 spawn 子进程 A（`run`，block_node=c）② 父读 stdout 至 `HANDSHAKE <trace_id> c` ③ 父 `kill -9` 子进程 A 并回收（验证 signal 退出）④ 父 spawn 子进程 B（`restore <trace_id>`，跑到完成） | 子进程 A 阶段：副作用文件 = [a, b]（c 未执行完）；磁盘存在 `next=c, steps_used=2` 检查点。子进程 B 后：副作用文件 = [a, b, c, d]（a/b 不重跑、c/d 各一次）；最终 state 正确 |
| T9 | 循环中途恢复：循环图 + 小 max_steps | 子进程 A 跑到中途（握手）→ kill -9 → 子进程 B 恢复 | 执行位置正确 + 步数预算延续（总步数到限才 `StepsExceeded`）+ **到限后下一节点未执行** |
| T10 | 双重恢复（R2 验收）：保存 → 新进程恢复 → 再保存 → 再次新进程恢复 | 子进程 A：run，block_node=c，握手后 kill -9；子进程 B：restore（c 执行、保存 next=d、block_node=d）握手后 kill -9；子进程 C：restore 跑到完成 | 副作用文件 = [a, b, c, d] 各一次；子进程 C 加载的是子进程 B 保存的 `next=d` 检查点（seq 延续，未回退） |

**node-Err 变体**（节点返回 Err 后恢复）保留为补充测试（进程内，快速），**不作为 R4 崩溃证据**。

**T8/T9/T10 承诺边界**（写进测试 doc comment）：验证「检查点**成功落盘后进程被强制终止** → 新进程恢复不重跑已提交节点」。**不能**证明「工具成功但保存前崩溃」不重复执行（需幂等键/去重，暂缓项）。

### 9.3 既有测试适配

- `checkpoint_test.rs` / `checkpoint_restore_test.rs`（15 个）：适配新格式（`format_version` / `next_node` / `steps_used`），存/取/hash 链路语义不变。
- `graph_test.rs` / `parallel_test.rs`：`run_inline` 旧签名保留 → 预期零改动（实施时确认）。

---

## 10. 提交拆分（3 个独立 commit，便于检查与回退）

| Commit | 内容 | 可编译性 |
|---|---|---|
| **C1 恢复实现** | Checkpoint（format_version=1）+ Codec 两段式严格加载；FrameInfo 修正 + CheckpointSink async + emit 后移 + 溢出安全预算检查；`run_inline_from` 内部入口（公共 `run_inline` 旧签名保留）；CheckpointSaveSink 同步保存 + KeepLatest(0) 拒绝 + CheckpointSaved 接线；FileBlobStore（seq + flush/rename）；新错误变体；`execute_stream_with_checkpoint` / `with_restore(graph, cp, trace_id, config)` 新签名 + 入口校验；T1-T10 全部测试；`examples/persistent_restore.rs`；README 边界说明 | System B 的两个 Sink 做最小适配（跟随新 FrameInfo/async 签名）保持编译 |
| **C2 旧 API 删除** | 删 `session.rs`（SessionCheckpoint/SessionCheckpointSink/ExecutionSession/SessionError）+ `FrameStack`/`Frame`/`MemorySink`；lib.rs 导出更新；README 迁移说明；CHANGELOG 破坏性删除记录 | C1 之后独立可编译 |
| **C3 策略清理** | 删 `TriggerPolicy` / deprecated `CheckpointPolicy` / `RetentionPolicy::TimeBased` / `with_trigger` / `with_policy`；`CheckpointConfig` 精简（含 `for_store` 便捷构造器）；CHANGELOG | C2 之后独立可编译 |

每个 commit 后：`cargo +1.88.0 test -p lellm-graph`（受影响 crate 优先）→ workspace 全量。

---

## 11. 文件清单（预估）

**C1**：`checkpoint/checkpoint_data.rs`（新格式 + 错误变体）、`checkpoint/checkpoint_codec.rs`（两段式加载）、`checkpoint/store.rs`（FileBlobStore，~300 行 < 400 上限）、`checkpoint/mod.rs`、`exec/execution_loop.rs`、`exec/execution_engine.rs`、`graph/graph_core.rs`（`run_inline_from`）、`test_executor.rs`、`error.rs`、`event.rs`、`src/bin/restore_probe.rs`（新）、`tests/restore_new_process.rs`（新）、`tests/checkpoint_test.rs`、`tests/checkpoint_restore_test.rs`、`examples/persistent_restore.rs`（新）、README×2

**C2**：`exec/session.rs`（删）、`exec/mod.rs`、`checkpoint/checkpoint_data.rs`、`lib.rs`、README×2、CHANGELOG

**C3**：`checkpoint/checkpoint_policy.rs`、`exec/execution_loop.rs`、`lib.rs`、CHANGELOG

> 文件夹文件数：`checkpoint/` 维持 7 个（FileBlobStore 并入 `store.rs`），不超 8 上限。`examples/` 新增 1 个。

---

## 12. 风险与开放点

| 风险 | 缓解 |
|---|---|
| seq「扫目录取 max+1」在并发写者下冲突 | 单写者约束文档明示（§6.3）；phase 2 评估锁 |
| flush+rename 被误读为断电安全 | README + 测试注释 + §6.2 写明「写入完成 ≠ 断电持久」两层区别 |
| 旧格式检查点无法加载 | 预期行为：`UnsupportedFormat` 明确报错，不静默降级 |
| `MemorySink` 若有内部测试使用 | 实施时核对；若有，C1 适配、C2 随 Frame 删除并改写测试 |
| prune 失败语义 | best-effort（`tracing::warn`），检查点落盘成功即视为越过边界 |
| kill -9 的跨平台（CI Linux / 本机 macOS） | 用 `kill -9 <pid>` 命令（两平台均有）；回收后验证 signal 退出状态 |
