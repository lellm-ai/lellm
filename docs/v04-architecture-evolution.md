# LeLLM v0.4 架构演进

> 版本：v0.4 | 日期：2026-06-18 | 状态：规划中
>
> 本文档记录 v0.3 → v0.4 的设计决策和演进路线。

## 目录

- [一、v0.3 收尾：消灭双来源状态](#一v03-收尾消灭双来源状态)
- [二、v0.4 核心：ReAct = 有环图](#二v04-核心理act--有环图)
- [三、v0.4+ 终局：Typed State + Effect 事件溯源](#三v04-终局typed-state--effect-事件溯源)
- [四、架构演进路线图](#四架构演进路线图)
- [五、架构演进对比](#五架构演进对比)

---

## 一、v0.3 收尾：消灭双来源状态

### 问题

当前 `ToolUseLoop` 持有私有 `LoopState`（`Vec<Message>`, `estimated_tokens`, `iterations` 等），
同时 Graph 层有自己的 `State = HashMap<String, Value>`。
**双来源状态 = Bug 温床。**

### 决策：方案 B+（统一状态来源）

`ToolUseLoop` 不再持有任何私有状态。所有 Agent 状态全部摊在 Graph State 中：

```rust
// 核心状态键收拢，成为底层图的公共契约
pub static SK_MESSAGES: StateKey<Vec<Message>> = StateKey::new("messages");
pub static SK_ITERATIONS: StateKey<u32> = StateKey::new("iterations");
pub static SK_TOOL_CALLS: StateKey<Vec<ToolCall>> = StateKey::new("tool_calls");
pub static SK_STOP_REASON: StateKey<StopReason> = StateKey::new("stop_reason");
pub static SK_OUTPUT_TOKENS: StateKey<usize> = StateKey::new("output_tokens");
pub static SK_REASONING_TOKENS: StateKey<usize> = StateKey::new("reasoning_tokens");
```

### 带来的质变

1. **单一事实来源（SSOT）**：外部系统通过 Checkpoint 观察 Agent 执行时，能清晰看到迭代轮次、Token 消耗等
2. **Agent 降维成 SubGraph**：`AgentFlowNode` 不再做复杂的内部编排，它自己就是由 LLM Node + Tool Node 组合的预制子图
3. **状态的确定性**：Checkpoint 保存的 State 与运行时看到的 State 完全一致

### 待做清单

- [ ] 从 `LoopState` 迁移所有字段到 Graph State keys
- [ ] `ToolUseLoop` 删除 `LoopState`，改为读写 Graph State
- [ ] `AgentFlowNode` 简化为 SubGraph 包装器
- [ ] 验证 Checkpoint 能正确恢复 Agent 中间状态

---

## 二、v0.4 核心：ReAct = 有环图

### 问题

当前 `ToolUseLoop` 是一个手写的 `while` 循环（`runtime.rs:303-394`）：
LLM 调用 → 检查 tool_calls → 执行工具 → 追加消息 → 回到 LLM。

### 决策：方案 B（中等粒度 Graph 建模）

```
[LLM Call] --有tool_calls--> [Execute Tools] --(自环)--> [LLM Call]
       --无tool_calls--> [End]
```

### 为什么不选其他方案

| 方案 | 描述 | 评价 |
|------|------|------|
| A（粗粒度） | 整个 ReAct 封装成单个节点，用自环替代 while | 过于敷衍，失去 Graph 能力 |
| **B（中等粒度）** | **LLM Node + Tool Node + 条件边** | **可观测性与灵活性的平衡** |
| C（细粒度） | 每步独立节点（LLM → Condition → Tool → Compactor） | 过度拆分，ReAct 内部紧密耦合 |

### 与现有 ToolUseLoop 的关系

**方案 B — 直接替换：**
- `ToolUseLoop` 内部不再手写 `while` 循环
- 构建内部 Graph（LLM Node → Condition → Tool Node → 自环）
- 调用 `GraphExecutor` 驱动循环
- `ToolUseLoop` 变成一层薄壳，API 签名不变（用户无感知）

### 内部 ReAct Graph 的 State 传递

**方案 B — State 承载关键数据，LoopState 被打散到 State 中：**

- `SK_MESSAGES` → 消息历史
- `SK_ITERATIONS` → 迭代计数
- `SK_TOOL_CALLS` → 本轮工具调用
- `SK_OUTPUT_TOKENS` → 累计输出 Token
- `SK_REASONING_TOKENS` → 累计推理 Token

`ToolUseLoop` 在构建内部 Graph 的 `initial_state` 时，将输入数据写入 State；
循环结束后从 State 读取回来构建 `ToolUseResult`。

### 嵌套结构

```
外部 Graph（用户编排）
  └── AgentFlowNode（Agent 适配为 Graph 节点）
        └── ToolUseLoop（薄壳）
              └── 内部 ReAct Graph（LLM ↔ Tool 循环）
```

### 待做清单

- [ ] 设计 `LLMNode` — 执行单次 LLM 调用，写入 messages 和 tool_calls 到 State
- [ ] 设计 `ToolNode` — 读取 tool_calls，执行工具，写入 results 到 State
- [ ] 设计 `ConditionNode` — 检查 tool_calls 是否为空，路由到 ToolNode 或 End
- [ ] `ToolUseLoop` 内部构建 ReAct Graph，替代 while 循环
- [ ] 验证流式输出与现有 `AgentStream` 兼容

---

## 三、v0.4+ 终局：Typed State + Effect 事件溯源

### 问题

v0.3 的 `HashMap<String, Value>` 是动态的、弱类型的。
`StateKey<T>` 和 `ReducerRegistry` 是补丁——在边界处做运行时类型检查。

### 终局愿景：Workflow<S> + Effect<S>

#### 核心 1：节点返回 Effect 而非 Delta

```rust
// 每个 Workflow 领域定义自己的 Effect
pub enum AgentEffect {
    AppendMessage(Message),
    IncrementIteration,
    RecordUsage(TokenUsage),
}

// 状态机作为纯函数应用 Effect
impl WorkflowState for AgentState {
    type Effect = AgentEffect;

    fn apply(&mut self, effect: Self::Effect) {
        match effect {
            AgentEffect::AppendMessage(msg) => self.messages.push(msg),
            AgentEffect::IncrementIteration => self.iterations += 1,
            AgentEffect::RecordUsage(usage) => self.usage += usage,
        }
    }
}
```

#### 核心 2：编译期 Merge 替代运行时 ReducerRegistry

```rust
pub trait Merge {
    fn merge(self, other: Self) -> Result<Self, WorkflowError>;
}

impl Merge for AgentState {
    fn merge(mut self, other: Self) -> Result<Self, WorkflowError> {
        self.messages.extend(other.messages);
        self.iterations = self.iterations.max(other.iterations);
        Ok(self)
    }
}
```

**零运行时字符串匹配开销。** 合并规则在编译期确定。

#### 核心 3：Checkpoint = Effect Log

- **持久化**：追加轻量级 Effect（如 `IncrementIteration`）到数据库，而非序列化几百 KB 的 JSON State
- **恢复**：重放 Effect Log，天然支持确定性重放测试（Deterministic Replay Testing）
- **可观测性**：每个 Effect 都是领域事件，天然可审计

---

## 四、架构演进路线图

```
  v0.3 (当前阶段: 大内聚/收拢)
  [消灭 LoopState] ──> [统一 StateKey (方案 B+)]
  [Agent 降维成 SubGraph] ──> [单一事实来源]
                                    │
                                    ▼
  v0.4 (破茧成蝶: 强类型领域)
  [ReAct = 有环图] ──> [砸碎 HashMap] ──> [Workflow<S>]
  [Agent 内部基于 Graph] ──> [Effect 事件溯源]
                                    │
                                    ▼
  v0.5 (多智能体时代)
  [Multi-Agent Orchestration] ──> [Durable Execution]
  [Agent ↔ Agent via MCP] ──> [Sampling]
```

---

## 五、架构演进对比

| 维度 | v0.3 务实形态 (方案 B+) | v0.4+ 终极形态 (Typed State) |
|------|------------------------|----------------------------|
| **状态底层** | `HashMap<String, serde_json::Value>` | 用户自定义强类型结构体 `S` |
| **类型安全** | 靠 `StateKey<T>` 在边界处做动态检查 | 纯编译期静态类型安全 |
| **变更机制** | `StateDelta` + `ReducerRegistry` | `Effect<S>` 纯函数重放 (`apply`) |
| **并行合并** | 运行时字符串匹配 Reducer | `Merge` trait 编译期确定 |
| **Checkpoint** | 全量/增量 State 快照 | Effect Log 重放 |
| **编排心智** | "我往一个共享的 KV 盒子里塞数据" | "我在驱动一个专属的领域状态机" |
| **可观测性** | State 快照 + 事件流 | Effect Log = 天然审计轨迹 |

---

## 六、关键设计决策

| 决策 | 结论 | 理由 |
|------|------|------|
| v0.3 是否引入 TypedState | 否 | HashMap 骨架已铺设，v0.3 聚焦收拢 |
| LoopState 去留 | 消灭 | 双来源 = Bug 温床 |
| ReAct 建模粒度 | 中等（LLM + Tool + 条件边） | 可观测性与灵活性的平衡 |
| ToolUseLoop 替换方式 | 内部替换，API 不变 | 用户无感知迁移 |
| v0.4 TypedState 时机 | v0.4 专门 grill | 范围大，需要独立规划 |
| Effect vs Delta | v0.4+ 用 Effect 取代 Delta | 事件溯源 > 状态补丁 |
