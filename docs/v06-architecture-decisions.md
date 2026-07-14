# LeLLM v0.6 架构决策记录

> 日期：2026-07-14 | 来源：Grilling Session

## 已确认决策

### 1. lellm-core 纯度 — 保持现状

**决策：** `ExecutableTool` 留在 `lellm-core`，不拆分。

**理由：**
- 分层清晰：协议层（`ToolDefinition`）+ 可执行描述（`ExecutableTool`）+ 构造框架（`tool` feature）+ 运行时（`lellm-agent`）
- `ExecutableTool` 只是"定义 + 执行器"的打包，不涉及调度
- `#[cfg(feature = "tool")]` 已隔离 schemars 依赖
- `lellm-agent` 硬编码 `features = ["anyhow", "tool"]` 是合理表达依赖关系

### 2. ReAct 图结构 — 固定，通过钩子扩展

**决策：** `AgentBuilder.build()` 内部固定构建 ReAct 图，用户不能替换节点。

**扩展点：** `FallbackStrategy`、`RetryPolicy`、`ContextBudget`、`ToolCatalog`、`StepCallback`、`RequestOptions`

**完全自定义：** 使用 `GraphBuilder`（世界二）

**文档：** 已更新 `AGENT-DESIGN.md`

### 3. Mutation as Runtime IR（C2 方案）

**决策：** Mutation 是 Graph Runtime 的唯一状态变更模型，但业务层不直接编写 Mutation。

**层次：**
```
业务代码 → ctx.append_message() → Mutation::AppendMessage → state.apply() → Checkpoint / Trace
```

**关键设计：**
- 每个 Mutation 一个类型（非 enum），实现 `StateMutation<S>`
- `WorkflowState::apply_batch()` → `apply()`（State 只负责单个）
- Mutation 自动生成（可选 derive 宏）
- 保留 Mutation 而非删除——Checkpoint、Parallel Merge、Barrier、Trace 的基础

**实施路径：**
- v0.5（现在）— Mutation trait 存在，AgentState 直接修改字段（临时兼容）
- v0.6 — 引入 `StateContext`，业务改为 `ctx.append_message()` 等 API
- v0.6 — Mutation 自动生成（可选 derive 宏）
- v0.7+ — 基于 Mutation 的 Checkpoint、Replay、Trace 完整落地

**文档：** 已更新 `AGENT-DESIGN.md` 和 `BLUEPRINT.md`

### 4. MCP Server Feature Gate

**决策：** MCP Server 代码通过 `server` feature 隔离。

**已完成的代码修改：**
- `lellm-mcp/src/protocol/error.rs` — `McpToolError` 添加 `#[cfg(feature = "server")]`
- `lellm-mcp/src/protocol/mod.rs` — `McpToolError` 导出添加 feature gate
- `lellm-mcp/src/lib.rs` — `McpToolError` re-export 添加 feature gate

**效果：** 仅使用 MCP Client 的用户不编译 Server 代码（无 axum 依赖）

### 5. ExecutionEngine 借用模型 — 保持

**决策：** `ExecutionEngine<'a, S>` 保持借用 `&'a mut S`，不转移所有权。

**理由：**
- 当前 Barrier 只发 pause 信号，不读取 State
- 外部通过独立的 State 副本或 Checkpoint 查看状态
- 所有权转移会让 API 变得复杂

### 6. FlowNode 渐进迁移 — 待实施

**决策：** 标记 `FlowNode` 为 deprecated，v0.6 移除，统一迁移到 `LeafNode`。

**理由：**
- `FlowNode` 的 `&mut S` 直接修改 State，绕过了 Mutation 模型
- `LeafNode` 的只读 + record Mutation 是正确方向
- 与 Mutation as Runtime IR 决策一致

**待做：**
- [ ] 在 `FlowNode` trait 上添加 `#[deprecated]` 标记
- [ ] 内部代码从 `FlowNode` 迁移到 `LeafNode`
- [ ] `NodeKind::External` 变体标记 deprecated
- [ ] v0.6 移除 `FlowNode` 和 `NodeContext`（保留 `LeafContext`）

## 未深入讨论

- MCP 工具冲突解决策略（`ConflictPolicy` / `NameConflictPolicy`）— 当前实现合理，暂无问题
- `lellm-derive` 宏设计 — 需后续评审
