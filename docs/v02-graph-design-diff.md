# v0.2 Graph 设计文档 vs 代码差距分析

> **日期：** 2026-06-15
> **原则：** 代码逻辑为准，文档追平代码

---

## [S1] 设计目标 — 核心分歧

| 项目 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| **DAG 类型** | 严格 Acyclic，不允许任意环 | **允许有环**，循环保护由 `max_steps` 运行时熔断提供 | **重大偏离** |
| **循环表达** | NodeKind::Loop 节点，不是边形成环 | 两种方式并存：LoopNode + `edge_if` 回跳 + `ConditionNode::Goto` 回跳 | **已扩展** |
| **执行模式** | 宏观串行，Parallel Node 内部并发 | 宏观串行，**ParallelNode 未实现** | **缺失** |

### 路线 B 决策

设计文档写的是"严格 DAG"，但 commit `27a0839` 明确选择了**路线 B**：

```
27a0839 refactor(graph): 路线B有环图 + 熔断器 + BarrierNode
```

`graph.rs:19` 的注释确认：

```rust
/// 图（Graph）— 允许有环，循环保护由 GraphExecutor::max_steps 运行时熔断提供。
```

**结论：** 代码已偏离设计文档，选择了有环图 + 熔断器。设计文档需要更新。

---

## [S2] Node 类型定义

### 节点种类对比

| 节点类型 | 设计文档 | 实际代码 | 差距 |
|---------|---------|---------|------|
| Task | ✅ | ✅ `TaskNode` | 一致 |
| Agent | ✅ `AgentNode { agent: ToolUseLoop }` | ✅ `AgentNode` + **新增 `LLMNode`** | LLMNode 是设计文档没有的 |
| Tool | ✅ `ToolNode { tool_name: String }` | ✅ `ToolNode { executor: ToolExecutor }` | **持有方式不同** |
| Condition | ✅ | ✅ `ConditionNode` | 一致 |
| Loop | ✅ | ✅ `LoopNode` | 一致 |
| **Barrier** | ❌ 无 | ✅ `BarrierNode` | **新增** |
| **LLM** | ❌ 无 | ✅ `LLMNode` | **新增** |

### AgentNode 差异

| 字段 | 设计文档 | 实际代码 |
|------|---------|---------|
| 结构体字段 | `name`, `agent` | `name`, `agent`, **`prefix`**, **`write_messages`**, **`write_stats`** |
| NodeKind 包裹 | `Agent(AgentNode)` | `Agent(Box<AgentNode>)` — **加了 Box** |
| State key | `messages_key`, `output_key` | **`prefix`** 统一前缀，自动拼接 `.messages` / `.output` / `.iterations` / `.tool_calls` / `.stop_reason` |

**结论：** AgentNode 实际实现比设计丰富得多，增加了 prefix 机制和统计写回。

### LLMNode（设计文档没有）

`llm_node.rs:260-340` 新增了 `LLMNode`：
- 单次 LLM 调用（不包装 ToolUseLoop）
- 配合 ToolNode + ConditionNode 手动构建 ReAct 循环
- 代码中有明确的警告注释，说明这是高级用法

**结论：** LLMNode 是设计文档遗漏的新节点类型，需要补充。

### ToolNode 差异

| 字段 | 设计文档 | 实际代码 |
|------|---------|---------|
| 工具引用 | `tool_name: String`（名字引用） | **`executor: ToolExecutor`**（直接持有） |
| execute() | stub（"Task 6 中完善"） | **完整实现** — 提取 tool_calls、执行、追加 ToolResult |

**结论：** ToolNode 从名字引用改为直接持有 ToolExecutor，且已完整实现。

### BarrierNode（设计文档没有）

`barrier_node.rs` 完整实现了 Human-in-the-loop：
- oneshot channel 等待外部决策
- 超时 + 默认行为（Approve/Reject/Skip）
- 4 种决策：Approve / Reject / Modify / Reroute
- 仅支持流式模式，阻塞模式直接报错

**结论：** BarrierNode 是设计文档遗漏的重要节点类型，需要补充。

### LoopNode Box 包裹

设计文档：`Loop(LoopNode)`
实际代码：`Loop(Box<LoopNode>)` — 加 Box 因 LoopNode 含 SubGraph，体积不确定

---

## [S3] Graph 结构

### 循环检测

| 项目 | 设计文档 | 实际代码 |
|------|---------|---------|
| `detect_cycle()` | ✅ 有，DFS 检测 | ❌ **已删除** |
| `validate()` | 含 cycle check | 只验证节点/边引用有效性 |

设计文档的 `validate()` 包含：

```rust
// 4. 检查无环（使用 DFS）
self.detect_cycle()?;
```

实际代码 `graph.rs:51-82` 只有节点和边的存在性校验。

**结论：** 循环检测已删除，因为图允许有环。

---

## [S4] State 设计

| 项目 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| `State` 类型别名 | ✅ | ✅ | 一致 |
| `GraphResult` | ✅ | ✅ | 一致 |
| `ExecutionEntry` | ✅ | ✅ | 一致 |
| **`StateReducer`** | ❌ | ✅ | **新增** |
| **`StateExt` trait** | ❌ | ✅ | **新增** |
| **`array_reducer()`** | ❌ | ✅ | **新增** |

**结论：** 代码新增了 Reducer 机制（`state.rs:23-100`），支持显式合并操作，类似 LangGraph 的 `operator.add`。设计文档需要补充。

---

## [S5] 执行语义

### 新增特性

| 项目 | 设计文档 | 实际代码 |
|------|---------|---------|
| **`max_steps` 全局熔断** | ❌ | ✅ `GraphExecutor::max_steps`（默认 50） |
| **流式执行 `execute_stream`** | ❌ | ✅ 完整实现，返回 `GraphStream` |
| **`GraphEvent` 事件体系** | ❌ | ✅ NodeStart/NodeEnd/Agent/BarrierPaused/GraphComplete/GraphError |
| **`find_next_node` 条件边优先级** | 未说明 | 条件边优先，无条件边 fallback |
| Parallel Node 并发 | ✅ 有设计 | ❌ **未实现** |

### `find_next_node` 逻辑差异

设计文档的 `find_next_node`：

```rust
// 无条件边优先，条件边"简化处理"
for edge in &edges {
    if edge.condition.is_none() {
        return Ok(edge.to.clone());
    }
}
// 条件边报错
Err(GraphError::InvalidGraph(...))
```

实际代码 `executor.rs:289-331`：

```rust
// 先评估条件边（按声明顺序）
for edge in &edges {
    if let Some(ref condition) = edge.condition
        && condition(state)
    {
        return Ok(edge.to.clone());
    }
}
// 无条件边作为 fallback
for edge in &edges {
    if edge.condition.is_none() {
        return Ok(edge.to.clone());
    }
}
// 都不匹配才报错，且附带所有条件评估结果
```

**结论：** 实际实现比设计文档完善得多——条件边优先评估，无条件边是 fallback，错误信息包含所有条件的评估结果。

### ParallelNode 缺失

设计文档 [S5] 有 ParallelNode 的完整设计（`futures::future::join_all` + Reducer 聚合），但代码中**完全没有实现**。

**结论：** ParallelNode 是设计有但代码缺失的功能。

---

## [S6] Builder API

| 项目 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| `edge_if` | ✅ 示例中有 | ✅ `GraphBuilder::edge_if()` | 一致 |
| ConditionNode 示例 | 用 inline branches | 用 `ConditionNode::builder()` | 一致 |

---

## [S7] 验证规则

| 项目 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| 单起点、单终点 | ✅ | ✅ | 一致 |
| 所有节点可达 | ✅ 提到 | ❌ **未实现** | 缺失 |
| 无环检测 | ✅ | ❌ **已删除** | 有环图不需要 |
| Edge 条件覆盖完整 | ✅ 提到 | ❌ **未实现** | 缺失 |

---

## [S8] 错误类型

| 变体 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| `InvalidGraph` | ✅ | ✅ | 一致 |
| `NodeNotFound` | ✅ | ✅ | 一致 |
| `NodeExecutionFailed` | ✅ `source: Box<dyn Error>` | ✅ `source: Box<dyn Error + Send + Sync>` | 加了 Send + Sync |
| `LoopLimitExceeded` | ✅ | ✅ | 一致 |
| `StateError` | ✅ | ✅ | 一致 |
| **`StepsExceeded`** | ❌ | ✅ | **新增** |
| **`BarrierTimeout`** | ❌ | ✅ | **新增** |
| **`BarrierCancelled`** | ❌ | ✅ | **新增** |

---

## [S9] Crate 结构

### 文件拆分

| 文件 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| `lib.rs` | ✅ | ✅ | 一致 |
| `error.rs` | ✅ | ✅ | 一致 |
| `state.rs` | ✅ | ✅ | 一致 |
| `node.rs` | 全部节点 | 仅 Task/Condition/Loop/SubGraph/NodeKind | **拆分了** |
| **`llm_node.rs`** | ❌ | ✅ AgentNode + LLMNode | **新增** |
| **`tool_node.rs`** | ❌ | ✅ ToolNode | **新增** |
| **`barrier_node.rs`** | ❌ | ✅ BarrierNode | **新增** |
| **`event.rs`** | ❌ | ✅ GraphEvent + BarrierDecision | **新增** |
| `graph.rs` | ✅ | ✅ | 一致 |
| `executor.rs` | ✅ | ✅ | 一致 |

commit `a70c719` 拆分了 node.rs：

```
a70c719 refactor(graph): 拆分 node.rs 为 llm_node.rs + tool_node.rs
```

---

## [S10] 与 v0.1 集成

| 项目 | 设计文档 | 实际代码 | 差距 |
|------|---------|---------|------|
| AgentNode 持有 ToolUseLoop | ✅ | ✅ | 一致 |
| LoopDetector/SignalVoter | "feature gate 集成" | 🔒 `v02-preview` feature gate，未默认开启 | 一致 |
| 复用 ToolExecutor | ✅ | ✅ | 一致 |

---

## [S11] 测试策略

| 测试场景 | 设计文档 | 实际代码 | 差距 |
|---------|---------|---------|------|
| 简单线性流水线 | ✅ | ✅ | 一致 |
| 条件分支 | ✅ | ✅ | 一致 |
| Loop 循环 + 熔断 | ✅ | ✅ | 一致 |
| 错误处理 | ✅ | ✅ | 一致 |
| **Parallel Node 并发** | ✅ | ❌ **未实现** | 缺失 |
| **有环图 + 熔断** | ❌ | ✅ 3 个测试 | **新增** |
| **Barrier 全流程** | ❌ | ✅ 6 个测试 | **新增** |
| ConditionNode 回跳 | ❌ | ✅ | **新增** |

---

## [S12] 版本路线图

| 版本 | 设计文档 | 实际状态 |
|------|---------|---------|
| v0.2 | Graph/Node/Edge + LoopDetector/SignalVoter | Graph 已完成，LoopDetector/SignalVoter 在 feature gate 后 |
| v0.3 | StateGraph（LangGraph 风格任意环）| **路线 B 已有任意环**，v0.3 的目标已被 v0.2 覆盖 |
| v0.4 | Checkpoint + 持久化 | 未开始 |

**重要发现：** 路线 B 的有环图已经实现了 v0.3 承诺的"LangGraph 风格任意环"。路线图需要重新规划。

---

## 总结：需要补充到设计文档的内容

### 新增概念（代码有，文档无）

1. **路线 B 决策** — 有环图 + `max_steps` 熔断器替代严格 DAG
2. **LLMNode** — 单次 LLM 调用节点，用于手动 ReAct 循环
3. **BarrierNode** — Human-in-the-loop 审批节点
4. **`GraphEvent` 事件体系** — 流式执行的事件穿透
5. **`execute_stream` 流式模式** — GraphExecutor 的流式执行入口
6. **`max_steps` 全局熔断器** — 运行时循环保护
7. **`StateReducer` / `StateExt`** — 显式状态合并机制
8. **ToolNode 完整实现** — 直接持有 ToolExecutor，非名字引用
9. **AgentNode prefix 机制** — 统一 key 前缀 + 统计写回
10. **`find_next_node` 条件边优先** — 条件边 > 无条件边 fallback

### 已删除功能（文档有，代码无）

1. **`detect_cycle()`** — 循环检测（因路线 B 不需要）
2. **ParallelNode** — 并行子图（设计有，代码未实现）
3. **可达性验证** — `validate()` 不检查所有节点是否可达
4. **Edge 条件覆盖验证** — `validate()` 不检查条件边是否覆盖完整

### 版本路线图影响

- v0.3 的"StateGraph（任意环）"已被 v0.2 路线 B 覆盖
- 需要重新定义 v0.3+ 的目标
