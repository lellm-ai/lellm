# LeLLM Agent Checkpoint Phase 2 设计

- **日期**：2026-10-02
- **性质**：讨论/评审文档（`discuss/`），非正式交付文档
- **状态**：设计定稿（待实施计划）
- **上游**：`discuss/lellm-architecture-audit-2026-09-30.md`（Q1 phase 2）、`discuss/lellm-restore-phase1-design.md`（Graph 恢复第一阶段）

---

## 0. 背景与目标

### 0.1 现状

- Graph 层已在恢复第一阶段具备 durable checkpoint + 恢复能力（串行图、进程崩溃安全、非 exactly-once），见 `lellm-restore-phase1-design.md`。
- 但 **Agent 主路径 `ToolUseLoop` 仍 `checkpoint=None`**（`lellm-agent/src/runtime/runtime.rs:137-143`）——最核心的用户路径（User → Agent → LLM → Tool → …）没有 durable 能力。
- 审计明确：「Agent 主入口若暂未接入，必须明确限制（文档 + API 层面）」。

### 0.2 目标

让 Agent（ReAct 图）复用 Phase 1 内联 checkpoint 基础设施，具备：

- 每节点同步保存检查点（opt-in）
- 从检查点恢复续跑（含步数 / 预算延续）
- 恢复完整性校验（禁止静默继续）

### 0.3 非目标

见 §11。

---

## 1. 关键架构发现：Agent 是图，复用内联路径

### 1.1 Agent 本身就是图

- `ToolUseLoop` 是薄 Facade，持有 `Arc<Graph<AgentState, AgentStateMerge>>`（预构建 ReAct 图）— `runtime.rs:94`。
- ReAct 图结构（`graph_builder.rs`）：

  ```text
  START → budget_check
  budget_check --budget_ok--> llm
           --need_compact--> compactor → llm
  llm → post_llm_check
  post_llm_check --budget_exceeded--> end
               --has_tool_calls--> tool → budget_check (循环)
               --no_tool_calls--> end
  ```

- 节点类型：`llm` / `tool` / `post_llm_check` / `budget_check` / `compactor` 全是 `ExternalLeaf`，`end` 是 `TaskNode`。**无 Parallel / Subgraph / Barrier**。
- 因此通过 `Graph::validate_persistable()`（`graph_core.rs:357`，只拒绝那三种节点）。

### 1.2 内联路径已支持 checkpoint + 恢复

- `run_inline_from(start_node, steps_used, max_steps, …)`（`graph_core.rs:338`）是首次运行与恢复的**统一入口**。
- 执行循环 `run_graph_loop`（`run_loop.rs:150`）在 commit + 路由解析后**同步** `emit_checkpoint(next, step)`；保存失败 → `CheckpointSaveFailed`，不越过边界。
- `ExecutionEngine::new(state, stream, cancel, checkpoint, barrier)`（`execution_engine.rs:228`）的 `checkpoint` 参数即 checkpoint sink；`ToolUseLoop` 现在传 `None`。
- **结论**：不需要新的执行机制，只需给 `ToolUseLoop` 注入 checkpoint sink + 暴露恢复入口。

### 1.3 SimpleExecutor 不可用

- `SimpleExecutor` 是「兼容层，供测试使用」，且**仅支持 `Graph<State, StateMerge>`**（默认泛型）— `test_executor.rs:31`。不能用于 `AgentState`。
- 其恢复校验（`validate_restore_checkpoint` + 最新性检查）私有且 `State`-specific。
- **决策**：把恢复校验**泛型化为共享 helper**（`Checkpoint<S>`），供 `SimpleExecutor` 与 Agent 复用（消除重复实现）。

---

## 2. AgentCheckpoint 三类语义

`AgentState::Checkpoint = AgentCheckpoint`（`typed_state.rs:170`）。按恢复契约分三类，防未来误删：

| 类别 | 字段 | 位置 | 说明 |
|---|---|---|---|
| **Durable State** | `messages` / `iterations` / `total_tool_calls` / `output_tokens` / `reasoning_tokens` / `compact_count` / `stop_reason` | `AgentCheckpoint` | 长期状态，恢复后继续累积 |
| **Execution Cursor** | `next_node` / `steps_used` / `graph_hash` | graph `Checkpoint` 包装层（`checkpoint_data.rs:95`） | 恢复位置 + 预算延续 + 结构指纹 |
| **Pending Context** | `last_response` | `AgentCheckpoint`（**本次新增**） | 当前 LLM turn 的结构化输出，ToolNode / PostLLMGuard 的输入 |

> **关键**：`last_response` 不是派生缓存，而是 **pending execution context**——execution cursor 之后、下一节点执行所需的上下文。

---

## 3. 核心设计问题：last_response 边界缺口

### 3.1 问题

`AgentCheckpoint` 当前**排除 `last_response`**（`typed_state.rs:185`，注释「可重建，下次 LLM 调用会填充」）。该假设隐含「checkpoint 永远落在下次 LLM 调用之前」，但 Agent checkpoint 的边界是**每个节点之后**，包括：

- `next_node = post_llm_check`：`PostLLMGuard` 读 `last_response` 判断 `has_tool_calls()` 路由 + 单轮推理预算（`guards.rs:94,116,158`）。
- `next_node = tool`：`ToolNode` 读 `last_response.tool_calls()` 执行工具（`tool_node.rs:51-56`）。

若这些边界恢复时 `last_response = None`：

- `PostLLMGuard`：`empty_response().has_tool_calls() = false` → **路由到 `end` 并报 `StopReason::Complete`**——提前假装成功结束，而 LLM 其实还想调工具。
- `ToolNode`：`tool_calls.is_empty()` → 静默跳过工具执行。

这是严重正确性缺陷（silent corruption）。

### 3.2 决策：存入 checkpoint

**`AgentCheckpoint` 增加 `last_response: Option<ChatResponse>`**，`snapshot()` 存入、`restore()` 直接使用。

理由（相对「从 messages 重建」）：

1. `last_response` 已是执行控制状态，不是 memory；`messages + iterations + tokens` 无法替代它（含 tool_calls / finish_reason / usage / provider metadata）。
2. 从 messages 重建引入隐藏不变量（tool_calls 不完全等价、provider metadata、未来 message 存储优化），脆弱。
3. checkpoint 本就是 snapshot，冗余可接受（类比 `graph_hash` 可重算但仍存，因它是恢复验证条件）。
4. **正确恢复 > 序列化体积**。

**不新增 `pending_tool_calls`**：`ChatResponse.tool_calls()` 已表达 ToolNode 所需输入，无需重复字段。

---

## 4. Q1：Checkpoint 边界

复用 Phase 1 的**每节点边界**（budget_check / llm / post_llm_check / tool / compactor 之后，commit + 路由后同步保存）。这比原「方案 A（LLM 与 tool 之间）/ 方案 B（仅 tool result 后）」都更细。

**关键不是选边界，而是让 checkpoint 在每个边界都自足**——即 §3 的 last_response 修复。修复后，任意节点边界恢复都能正确续跑。

---

## 5. Q2：保存什么 + 恢复完整性校验

### 5.1 保存内容

见 §2 三类语义。`AgentCheckpoint` 新增 `last_response: Option<ChatResponse>`。

### 5.2 恢复完整性校验（Agent 层）

恢复入口在**执行任何节点、写入任何新检查点之前**完成校验：

| next_node | last_response 要求 | 缺失时 |
|---|---|---|
| `post_llm_check` | 必须 `Some` | `MissingExecutionContext` |
| `tool` | 必须 `Some` | `MissingExecutionContext` |
| `budget_check` / `llm` / `compactor` | 不依赖（下次是 LLM 调用） | 允许 `None` |
| `end` | 不依赖（终态 TaskNode，无输入） | 允许 `None` |
| `None`（完成态） | 执行不依赖；**结果构造依赖**（§5.4） | 见 §5.4 |

> 其他位置「按实际依赖判断」——当前 ReAct 图只有 `post_llm_check` / `tool` 依赖 `last_response`。

### 5.3 旧检查点兼容 vs 执行上下文缺失

- 旧 `AgentCheckpoint`（无 `last_response` 字段）反序列化时按 `None` 加载（`Option` 缺省）。
- 但**必须经过 §5.2 的节点相关校验**：恢复到 `post_llm_check` / `tool` 且 `last_response == None` → 报错；其他位置按依赖判断；完成态零执行。
- **直接构造的检查点**（不经存储加载）也必须经过同样校验。
- 即：「旧检查点兼容」≠「跳过校验」，而是「按 `None` 加载 + 节点相关校验」。

### 5.4 完成态结果构造契约

区分「无需继续执行节点」与「能正确构造 `ToolUseResult`」：完成态（`next_node = None`）允许缺少 `last_response`（无节点再读它），但**结果构造仍依赖它**。

`ToolUseResult.response` 取自 `AgentState.last_response`（`runtime.rs:161-168`），当前 `None` 时回退 `empty_response()`。若旧完成态检查点缺 `last_response`，直接恢复会得到**空 response，丢失最终回答**。

**契约**：完成态恢复时——

- `last_response` 存在 → `response = last_response`（完整保真）。
- `last_response` 缺失（旧检查点）→ **从 `messages` 的最后一条 assistant 消息重建 `response`（文本内容）**，保证最终回答不丢失。此重建在完成态安全：循环结束于 PostLLMGuard 判定「无 tool calls」，故最后一条 assistant 消息即最终回答。
- 保真度说明：重建的 `response` 仅含文本，丢失 finish_reason / usage / provider metadata；**权威最终回答始终在 `messages`**（已入 checkpoint），调用方亦可从 `messages` 读取。

---

## 6. Q3：Tool 崩溃窗口（exactly-once 边界）

### 6.1 崩溃窗口

```text
LLM 返回 tool_calls → checkpoint(next=tool) 保存成功
→ ToolNode 执行工具（副作用）
→ 崩溃（checkpoint 尚未保存 tool 结果）
→ 恢复：next=tool → 重新执行工具 → 副作用重复
```

### 6.2 本阶段立场

- **不承诺 exactly-once**：工具成功但保存前崩溃 → 恢复重跑该 tool 节点。与 Phase 1 一致。
- **明确 tool replay 风险**：文档 + API doc 明示。
- **tool identity 已持久化**：assistant 消息的 `ContentBlock::ToolCall.id` + tool result 的 `tool_call_id` 都在 `messages`（已入 checkpoint），为未来幂等键 / 结果去重留锚点。
- exactly-once（幂等键 / 结果去重 / 补偿）是独立里程碑，本阶段不做。

---

## 7. API 设计

### 7.1 两个非流式入口（opt-in）

```rust
impl ToolUseLoop {
    /// 首次执行 + 每节点同步保存检查点。
    /// `trace_id` 由调用方预提供（崩溃后可定位检查点）。
    pub async fn invoke_with_checkpoint(
        &self,
        messages: Vec<Message>,
        trace_id: TraceId,
        config: CheckpointConfig<AgentState>,
    ) -> Result<ToolUseResult, LlmError>;

    /// 从检查点恢复续跑，并继续保存到同一 trace。
    /// 不接收新 messages（避免恢复时隐式追加输入）。
    pub async fn invoke_with_restore(
        &self,
        checkpoint: Checkpoint<AgentState>,
        trace_id: TraceId,
        config: CheckpointConfig<AgentState>,
    ) -> Result<ToolUseResult, LlmError>;
}
```

- 现有 `invoke` / `invoke_stream` 行为**完全不变**。
- `config: CheckpointConfig<AgentState>`（lellm-graph），含 store + codec + graph_hash + retention。
- 返回类型沿用 `LlmError`（见 §9 错误映射）。

### 7.2 四个约束

1. **trace_id 调用方预提供**：内联路径无内建 trace 生成机制（`CheckpointSaveSink` 构造时固定 trace_id，`checkpoint_save_sink.rs:30`）。若只在正常完成的 `ToolUseResult` 返回 trace_id，进程中途崩溃时调用方无法知道加载哪个检查点。故 `trace_id` 作为入参，调用方预先持有。
2. **恢复继续保存**：同一 trace 延续检查点序号（FileBlobStore seq）与 **Graph 步数预算**（`max_steps` 为图节点执行总预算，从 `steps_used` 延续，恢复不重置）。**Agent 业务预算**（`iterations` / `total_tool_calls` / token 计数，属 Durable State）随 checkpoint 快照独立延续，与 Graph 步数预算相互独立。恢复入口不收新 messages。
3. **校验分层**：泛型 helper 管通用规则（版本 / graph_hash / 游标 / 步数 / 最新性）；Agent 层管节点输入完整性（`last_response`）。**graph 层不识别 `post_llm_check` / `tool` 等 Agent 节点名**。
4. **配置兼容**：见 §10。

### 7.3 trace_id 绑定规则

两个入口的 `trace_id` 均由调用方提供，绑定规则：

1. **首次执行**（`invoke_with_checkpoint`）：要求该 `trace_id` **没有既有检查点**（`store.load_latest(trace_id)` 为空），避免新状态接在旧执行历史后。已有检查点 → 拒绝（提示改用 `invoke_with_restore`）。
2. **恢复**（`invoke_with_restore`）：要求传入检查点**属于该 `trace_id`** 且是**其最新检查点**（与 `store.load_latest(trace_id)` 一致），否则 `NotLatest`。
3. **并发**：沿用 Phase 1「同一 trace 单活跃执行者」约束；最新性检查本身**不提供并发互斥**（两个执行者同时读最新仍可能都通过），并发安全由调用方保证。

---

## 8. 恢复校验分层

### 8.1 共享泛型 helper（graph 层）

从 `SimpleExecutor` 提取为 `Checkpoint<S>` 通用：

- `validate_restore_checkpoint<S, M>(graph, cp, max_steps) -> Result<(), GraphError>`：版本 / graph_hash / next_node 存在 / 步数边界。
- 最新性检查（需 store I/O，async）：`cp` 必须是该 trace 最新，否则 `RestoreNotLatest`。
- `SimpleExecutor` 与 Agent 都调用此 helper（消除重复）。

### 8.2 Agent 层校验

- `last_response` 节点相关完整性（§5.2）→ `MissingExecutionContext`。
- 在共享 helper 通过后、执行节点前完成。

---

## 9. 错误归属与对外映射

### 9.1 归属

- 通用恢复失败（版本 / 指纹 / 游标 / 步数 / 最新性）→ graph 层 `TerminalError` 既有变体（`RestoreUnsupported` / `RestoreNotLatest` / `RestoreFailed` / `StepsExceeded` / `NodeNotFound`）。
- `MissingExecutionContext` → **Agent 层**校验产生，graph helper 不判断 Agent 节点名。
- **运行期保存失败**（`CheckpointSaveFailed`，执行中发生）→ **非恢复失败**，不属 `RestoreFailed`，沿用 `LlmError::Provider`（见 §9.2）。

### 9.2 对外映射（决策）

两个入口返回 `Result<ToolUseResult, LlmError>`。为保留**可供程序识别**的恢复失败原因（不能只剩字符串），**新增 `LlmError` 变体**，**仅表达恢复校验失败**：

```rust
LlmError::RestoreFailed {
    reason: RestoreFailureReason,  // 类型化，可 match
    message: String,
}
```

`RestoreFailureReason`（lellm-core，`#[non_exhaustive]`）——**仅恢复校验失败**：

```rust
#[non_exhaustive]
pub enum RestoreFailureReason {
    MissingExecutionContext,  // Agent 层：next_node 依赖 last_response 但缺失
    NotLatest,               // 传入检查点非该 trace 最新
    GraphMismatch,           // graph_hash 不匹配
    UnsupportedFormat,       // 版本/格式不支持
    StepsExceeded,           // 步数预算耗尽
    NodeNotFound,            // next_node 不存在
    Other,                   // 兜底
}
```

**运行期保存失败 ≠ 恢复失败**：首次 `invoke_with_checkpoint` 或恢复执行中的 `CheckpointSaveFailed` 都**未发生恢复校验**，不属 `RestoreFailed`；沿用现有映射 `LlmError::Provider { provider: "react_graph", .. }`（`runtime.rs:149-154`），不新增恢复语义。

- 映射来源：graph helper 的恢复 `TerminalError` 变体（`RestoreNotLatest` / `RestoreFailed` / `StepsExceeded` / `NodeNotFound`）+ `CheckpointStoreError`（`GraphMismatch` / `UnsupportedFormat`）+ Agent 的 `MissingExecutionContext`，统一映射到 `RestoreFailureReason`。
- **API 影响**：`LlmError` 新增变体（pre-1.0 可接受）+ 新增 `#[non_exhaustive]` 公开 enum `RestoreFailureReason`。既有 `LlmError` 是否加 `#[non_exhaustive]` 属另一兼容性决定，本轮不扩大修改。

---

## 10. 配置兼容

- 自动校验沿用：版本 / graph_hash / 游标 / 步数 / 最新性。
- **model / tools / budget 相同由调用方保证**；第一版**暂不自动检测配置漂移**。
- 明确：`graph_hash` 一致只保证**图结构**兼容，**不等于**模型、工具实现、预算等执行语义兼容。工具名称相同也不能证明实现相同。
- 配置指纹方案（若未来需要自动检测漂移）**另行定义**，本阶段不做。

---

## 11. 范围与不承诺

**本阶段做**：

- `AgentCheckpoint` 增加 `last_response`（pending context）
- 恢复完整性校验（Agent 层 `MissingExecutionContext` + 共享泛型 helper）
- `ToolUseLoop::invoke_with_checkpoint` / `invoke_with_restore`（两个非流式入口）
- `LlmError::RestoreFailed` + `RestoreFailureReason`（`#[non_exhaustive]`，仅恢复校验失败）
- 四组必测场景（§12，含运行期保存失败接线）

**本阶段不做**：

- 持久化流式 API（`invoke_stream` 的 checkpoint 版）——留到有明确需求
- exactly-once 工具执行（幂等键 / 结果去重 / 补偿）
- HITL / barrier 接入 Agent（独立 Q2 工作）
- 断电安全（fsync）
- Parallel / Subgraph / Barrier 图恢复
- 配置漂移自动检测

---

## 12. 测试计划（四组必测场景）

> 使用**真实 snapshot / 序列化 / 加载链路**（非内存对象直接传递），走 `CheckpointConfig::for_store` + `SerdeCheckpointCodec<AgentState>` + store。

### 组 1：LLM 后恢复仍执行工具（覆盖两个恢复位置）

- **1a `next_node = tool`**：运行到 LLM 后（有 tool_calls）的检查点 → 恢复 → 断言 ToolNode 执行工具、**不重新调用已完成的 LLM**、tool result 注入、继续到下一轮。
- **1b `next_node = post_llm_check`**：运行到 post_llm_check 的检查点 → 恢复 → 断言 PostLLMGuard 正确读取 last_response（路由到 tool，而非误判 Complete）。
- 两个位置分别验证，避免漏掉 guard 对响应的依赖。

### 组 2：恢复后再次保存及恢复（区分两类预算）

- 恢复 → 继续执行 → 再保存 → 再恢复。断言：
  - 检查点序号（FileBlobStore seq）延续
  - **Graph 步数预算**延续：`steps_used` 从恢复点延续，`max_steps` 总预算不重置（不重新获得完整图执行额度）
  - **Agent 业务预算实际生效**：至少一个现有业务预算（如 `max_iterations`）在恢复后确实触发限制——恢复不重置 `iterations`，达到上限仍按原逻辑停止（而非恢复后重新获得完整业务额度）

### 组 3：缺失必要响应直接报错

- `next_node ∈ {post_llm_check, tool}` 且 `last_response == None` → 断言返回 `LlmError::RestoreFailed { reason: MissingExecutionContext, .. }`，**不静默继续**。
- 覆盖：存储加载的旧检查点 + 直接构造的检查点，两条路径都报错。
- 对照：`next_node` 为不依赖 last_response 的位置 + `last_response == None` → 正常恢复（不误报）。

### 组 4：运行期保存失败接线

- 构造使 `CheckpointSaveSink` 保存失败的 config（指向无效/只读路径的 store，或注入失败 save_fn）。
- 断言：`invoke_with_checkpoint` 返回错误（`LlmError::Provider { provider: "react_graph" }`，**非** `RestoreFailed`），且保存失败边界**之后**的 LLM/工具节点**未执行**（执行在该边界停止，不越过）。

### 验证证据限定

- **Agent 新进程 kill 测试本轮不重复建设**：前提是 Graph Phase 1 的真实进程崩溃测试（T8/T9/T10，`restore_new_process.rs`）仍保留。
- Agent 测试须覆盖**磁盘往返**（FileBlobStore 存/取）与**内联恢复接线**（`run_inline_from` + checkpoint sink）。
- 新进程崩溃恢复的进程级证据由 Graph Phase 1 测试承担；Agent 层证明「接线正确 + 磁盘往返正确」。

---

## 13. 风险与开放问题

| # | 项 | 处理 |
|---|---|---|
| R1 | `LlmError` 新增变体的破坏面 | `LlmError`（`lellm-core/src/error.rs:46`）当前**非 `#[non_exhaustive]`**，新增 `RestoreFailed` 变体对穷举 match 是破坏性变更。pre-1.0 可接受。既有 `LlmError` 是否加 `#[non_exhaustive]` 属另一兼容性决定，本轮不扩大修改；新 `RestoreFailureReason` 直接 `#[non_exhaustive]` |
| R2 | `SerdeCheckpointCodec<AgentState>` 实例化 | **已确认**：`AgentState` 已 derive `Serialize + Deserialize`（`typed_state.rs:27`），codec 边界 `S: Serialize + Deserialize` 满足，可直接实例化。实际序列化载荷是投影 `AgentCheckpoint`（非完整 runtime state）；新增 `last_response: Option<ChatResponse>` 可序列化（`ChatResponse` 已被 `AgentState.last_response` 使用） |
| R3 | 配置漂移 | 第一版文档约束，不自动检测（§10） |
| R4 | 流式 checkpoint | 本阶段不做，留待需求 |

---

## 14. 后续（本阶段之后）

- **HITL / barrier 接入 Agent**（Q2 phase 1b/2）：Barrier Decision Application Contract（HumanDecisionRecord，不污染 State KV）。
- **持久化流式 API**：`invoke_stream` 的 checkpoint 版。
- **exactly-once**：幂等键 / 结果去重 / 执行日志 / 补偿（独立里程碑）。
- **配置指纹**：自动检测配置漂移（若需要）。

---

## 15. 记忆更新待办（结论确认后执行）

- 新增：Agent checkpoint phase 2 设计（复用内联路径 / last_response pending context / 恢复完整性校验 / 两非流式入口）。
- 更新：`v04-progress-status`（Agent 主路径 checkpoint 从「未接入」推进到「phase 2 设计定稿」）。
- 关联：[[restore-phase1-done]]、[[checkpoint-three-layer-architecture]]。
