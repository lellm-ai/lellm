# ADR-0003: LeafContext / ExecutorOperation 执行模型分裂

**日期:** 2026-06-29
**状态:** Accepted
**作者:** cunge
**相关:** [ADR-0002](./ADR-0002-unified-execution-path.md), [[v04-execution-model-redesign]], [[grilling-leaf-context-decision]]

---

## 背景

v0.4 重构引入了 `ExecutionEngine` + `ExecutorState` 统一执行模型（ADR-0002），
将 `ExecutionContext` 重命名为 `ExecutionEngine`，确立了 Mutation 缓冲 + commit 模式。

在此基础之上，进一步细化了节点的能力边界：
- **Leaf 节点**（业务逻辑）只需要读 State + emit Mutation
- **Composite 节点**（执行控制）需要 clone/merge/replace_state 等完整能力

原有的 `FlowNode` / `NodeContext` 无法在编译期区分这两种需求。

## 决定

### 1. 引入 `LeafContext` — 纯借用视图

```rust
pub struct LeafContext<'a, S: WorkflowState> {
    state: &'a S,                          // 只读引用
    stream: Option<&'a dyn StreamSink>,
    cancel: &'a CancellationToken,
    control: &'a mut ExecutionControl,
    metadata: &'a mut NodeMetadata,
    mutations: &'a mut Vec<S::Mutation>,   // 借用在 Engine 的 buffer
    flow_events: &'a mut Vec<FlowEvent>,
}
```

与 `NodeContext` 的关键区别：
- `state` 字段为 `&S`（只读），`NodeContext` 为 `&mut S`
- 不提供 `replace_state()` 方法
- 编译期保证 Leaf 节点无法修改 State

### 2. 引入 `LeafNode` trait

```rust
#[async_trait]
pub trait LeafNode<S: WorkflowState = State>: Send + Sync {
    async fn execute(&self, ctx: &mut LeafContext<'_, S>) -> Result<(), GraphError>;
}
```

与 `FlowNode` 的区别：
- 接收 `LeafContext`（只读）而非 `NodeContext`（可变）
- 语义上表达"此节点只做声明式业务逻辑"

### 3. `NodeKind` 新增 `ExternalLeaf` 变体

```rust
pub enum NodeKind<S, M> {
    Task(...),
    Condition(...),
    Barrier(...),
    Parallel(...),
    External(Arc<dyn FlowNode<S>>),      // 向后兼容
    ExternalLeaf(Arc<dyn LeafNode<S>>),  // 新增
}
```

三个执行循环（`graph.rs`, `execution_loop.rs`, `test_executor.rs`）统一 match dispatch：
- `External` → `build_node_context()` → `FlowNode::execute()`
- `ExternalLeaf` → `build_leaf_context()` → `LeafNode::execute()`

### 4. `ExecutorOperation` 保留给 Composite 节点

```rust
#[async_trait]
pub trait ExecutorOperation<S: WorkflowState = State>: Send + Sync {
    async fn execute(&self, engine: &mut ExecutionEngine<S>) -> Result<(), GraphError>;
}
```

直接接收 `&mut ExecutionEngine`，拥有完整能力（clone/merge/replace_state/spawn_child）。

## 职责边界

```
Graph (AST)
    └── NodeKind (不实现任何执行 trait)

ExecutionEngine (runtime owner)
    ├── dispatch → match NodeKind
    ├── build_leaf_context() → LeafNode
    ├── build_node_context() → FlowNode (backward compat)
    └── pass &mut self → ExecutorOperation

LeafNode
    └── 只能 emit Mutation (LLM, Tool, Guard, Compactor)

ExecutorOperation
    └── 可以操纵 Executor (Parallel, Retry, Loop, SubGraph)
```

## 已迁移的节点

| 节点 | 原实现 | 新实现 |
|------|--------|--------|
| LLMNode | `FlowNode<AgentState>` | `LeafNode<AgentState>` |
| ToolNode | `FlowNode<AgentState>` | `LeafNode<AgentState>` |
| PostLLMGuard | `FlowNode<AgentState>` | `LeafNode<AgentState>` |
| CompactorNode | `FlowNode<AgentState>` | `LeafNode<AgentState>` |
| BudgetCondition | `FlowNode<AgentState>` | `LeafNode<AgentState>` |

## 向后兼容

| 类型 | 状态 |
|------|------|
| `FlowNode` trait | 保留，`NodeKind::External` 继续使用 |
| `NodeContext` | 保留，`build_node_context()` 继续可用 |
| `GraphNode` alias | 保留，指向 `dyn FlowNode` |
| `BarrierNode` | 仍实现 `FlowNode`，后续迁移 |
| `AgentFlowNode` | 仍实现 `FlowNode`，后续迁移 |
| `TaskNode` | 仍实现 `FlowNode`，后续迁移 |
| `ConditionNode` | 仍实现 `FlowNode`，后续迁移 |

## 影响

### 正面
- **编译期安全**：Leaf 节点无法修改 State，编译器保证
- **意图清晰**：`LeafNode` 一眼看出是业务逻辑节点
- **零运行时开销**：borrowed view，零分配
- **渐进式迁移**：`External` + `ExternalLeaf` 并存

### 负面
- **API 表面积增加**：新增 `LeafNode` trait + `ExternalLeaf` 变体
- **学习曲线**：需要理解 Leaf vs Composite vs ExecutorOperation 的区别

## 后续

- [ ] 迁移 `BarrierNode` → `LeafNode`
- [ ] 迁移 `AgentFlowNode` → `LeafNode`（需评估其复杂逻辑）
- [ ] 迁移 `TaskNode` → `LeafNode`
- [ ] 迁移 `ConditionNode` → `LeafNode`
- [ ] 考虑将 `FlowNode` 标记为 `#[deprecated]`
