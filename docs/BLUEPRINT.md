# LeLLM 产品蓝图

> 更新：2026-10-19 | 架构方向：Graph is Runtime, Agent is DSL
> 原始 v0.5 设计见 [v05-graph-as-runtime.md](./v05-graph-as-runtime.md)；当前优化计划见 [运行时恢复能力计划](../discuss/lellm-runtime-reliability-plan.md)。

## 当前优化主线

**已确认顺序：审批可恢复 → 工具重放边界 → 子图和并行恢复 → 多 Agent 模板。** M1–M4 首个完整切片已实现，尚未发布；使用与迁移见 [可恢复执行](./durable-execution.md)。依据为 [LangGraph 当前调研及本项目源码对照](./langgraph-current-research.md)。

| 阶段 | 交付目标 | 完成条件 |
| --- | --- | --- |
| M1 审批可恢复 | 持久化暂停、审批查询、决策去重与单写者保护 | 审批期间进程被终止，重启仍等待同一请求；重复决策不重复推进 |
| M2 工具重放边界 | 稳定任务身份、逐工具完成收据、幂等键与未知结果策略 | 已保存结果不重做；外部成功但未保存的窗口按声明策略处理 |
| M3 子图和并行恢复 | 子图身份与位置、能力继承、分支结果保存、父级一次提交 | 子图审批可恢复；成功分支复用；预算、取消、事件路径一致 |
| M4 多 Agent 模板 | PlannerWorkersReviewer 固定拓扑模板 | Worker 工具对账后复用成功分支，Reviewer 人工审批可重开存储续跑 |

新 `DurableRuntime` 复用 Graph 执行循环，支持审批、逐工具收据、嵌套子图和并行分支恢复。MemoryRunStore / FileRunStore 强制每个 run_id 独占执行；文件后端限定本机进程崩溃恢复。旧 format=1 checkpoint / restore 入口继续保留串行能力与原组合限制。

保留类型化状态、Mutation 与借用式 ExecutionEngine。先完善同步持久化；异步保存、增量快照、Compiler InlinePass 和分布式调度按实际需求及测量结果另行规划。部署限制和普通图缺审批处理器时的行为变更见使用与迁移文档。

## 一、项目愿景

做 Rust 版本的 LangChain / LangGraph / AutoGen。

- LLM 抽象层，标准化消息内容格式；提供基础的 LLM provider 适配
- 低层编排层，让开发者能精准控制 Agent 的执行流程；提供基础的 function call, agent loop, tool use, MCP client
- 支持节点 node, 边 edge, 图 graph, Multi-Agent Orchestration
- 支持流式输出、持久化执行、短期记忆、人类介入（human-in-the-loop）

## 二、架构方向

以下保留原始设计的分层与目标 API；PlannerBuilder、SupervisorBuilder、Compiler InlinePass 属于规划。示例用于说明设计，具体可用签名以源码与各阶段可编译示例为准。

### 核心论点

**Graph 是唯一的 Runtime，Agent 是 Graph 的 DSL / 模板构建器。**

### 6 Crate 架构

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

### Crate 职责

| Crate | 领域 | 职责 | 依赖 |
|-------|------|------|------|
| `lellm-core` | Protocol | 纯协议层：Message, ToolCall, Request/Response, LlmError | serde, thiserror |
| `lellm-graph` | Execution | 图编排引擎：Graph, Node, Edge, State, Checkpoint, ExecutionEngine | core |
| `lellm-provider` | Inference | LLM 调用：LlmProvider, CodecProvider, 三权分立 | core |
| `lellm-agent` | Agent | 智能体：AgentBuilder, ToolUseLoop, AgentState | core, graph, provider |
| `lellm-mcp` | Protocol | MCP 协议：McpClient, McpTransport | core |
| `lellm-derive` | Technical | 派生宏：#[tool], #[derive(Tool)] | 无 |

### 红线

1. `graph ↛ agent` — Graph 是通用引擎，Agent 是上层消费者
2. `provider ↛ graph` — Provider 只负责 LLM 调用
3. `mcp ↛ agent` — MCP 是独立协议域

### Feature Gate

```toml
[features]
default = ["provider"]
core = ["dep:lellm-core"]
provider = ["dep:lellm-core", "dep:lellm-provider"]
graph = ["dep:lellm-core", "dep:lellm-graph"]
agent = ["dep:lellm-core", "dep:lellm-graph", "dep:lellm-provider", "dep:lellm-agent"]
mcp = ["dep:lellm-core", "dep:lellm-graph", "dep:lellm-mcp"]
derive = ["dep:lellm-derive"]
full = ["graph", "provider", "agent", "mcp", "derive"]
```

## 三、Workspace 结构

```
lellm/
├── Cargo.toml                  # workspace root
├── lellm/                      # 门面 crate — feature-gated re-export
├── lellm-core/                 # 协议层，零运行时依赖
├── lellm-graph/                # 图编排引擎 + State + Checkpoint + ExecutionEngine
├── lellm-provider/             # LLM Provider trait + 适配器
├── lellm-agent/                # Agent 运行时 (AgentBuilder, ToolUseLoop)
├── lellm-mcp/                  # MCP 协议实现
├── lellm-derive/               # 派生宏
└── docs/                       # 文档
```

## 四、架构总览

```
用户层
──────────────────────────────────────
AgentBuilder  PlannerBuilder  SupervisorBuilder
                    (DSL, build() → Arc<Graph<S>>)

                    ↓ build()
               Arc<Graph<AgentState>>

                    ┌──────────────────────┐
                    │  ToolUseLoop (Facade) │  ← 薄层，持有 Arc<Graph>
                    │  invoke()             │     invoke_stream()
                    │  invoke_stream()      │
                    └──────────────────────┘

                    ↓ 组合

               SubgraphSpec (Engine 行为，不是 Node)

──────────────────────────────────────
          Runtime (lellm-graph)
ExecutionEngine  Node  Edge  State  Graph  Checkpoint

──────────────────────────────────────
          Primitive (lellm-graph)
GraphBuilder  (AST 构建器，build() → Graph)

──────────────────────────────────────
          Compiler (lellm-graph, 可选优化)
Inline Pass  (自动 merge Subgraph，用户不需要手动调用)
```

### 两层世界划分

**世界一：Runtime DSL（目标接口）**
- AgentBuilder
- PlannerBuilder
- SupervisorBuilder
- 特点：API 稳定，Runtime 可升级，内部结构不承诺

**世界二：Graph Primitive（完全自由）**
- GraphBuilder
- Node
- Edge
- Condition
- 特点：完全透明，想怎么改怎么改

## 五、核心 API

### 5.1 LlmProvider

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError>;
    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError>;
    fn provider_id(&self) -> &str;
}
```

### 5.2 WorkflowState

```rust
pub trait WorkflowState: Clone + Send + Sync {
    /// 可序列化的 Checkpoint 快照（projection，不是 raw state）
    type Checkpoint: Serialize + DeserializeOwned + Clone + Send;
    /// 状态变更命令
    ///
    /// **关键设计：** Mutation 是 Runtime 的内部 IR，不是业务层 API。
    /// 业务代码通过 `StateContext` 便捷方法操作（`ctx.append_message()`），
    /// ctx 内部生成 Mutation → state.apply()。
    type Mutation: StateMutation<Self>;

    /// 创建 checkpoint 快照
    fn snapshot(&self) -> Self::Checkpoint;
    /// 从 checkpoint 恢复
    fn restore(checkpoint: Self::Checkpoint) -> Self;
    /// 应用单个 Mutation（batch 是 Runtime 的事）
    fn apply(&mut self, mutation: Self::Mutation);
}
```

**Mutation as Runtime IR 层次：**
```
业务代码 → ctx.append_message() → Mutation::AppendMessage → state.apply() → Checkpoint / Trace
```
- Graph Runtime 只认识 Mutation（Checkpoint、Parallel Merge、Barrier、Trace 的基础）
- 业务代码永远写 State 操作（通过 `StateContext` 便捷 API）
- Mutation 自动生成（可选 derive 宏），不手写 enum

### 5.3 AgentBuilder (DSL)

```rust
// 构建 Graph — 推荐方式
let graph: Arc<Graph<AgentState>> = AgentBuilder::new(model)
    .system("你是一个助手")
    .tools([search_tool, weather_tool])
    .max_iterations(20)
    .build();

// 便捷执行 — Facade
let result = AgentBuilder::new(model)
    .tools([search_tool, weather_tool])
    .compile()
    .invoke(messages)
    .await?;
```

### 5.4 GraphBuilder (Primitive)

```rust
let graph = GraphBuilder::<MyState, _>::new("workflow")
    .start("fetch")
    .node("fetch", fetch_node)
    .node("process", process_node)
    .edge("fetch", "process")
    .end("process")
    .build()?;

// 执行
let mut engine = ExecutionEngine::new(&mut state, None, CancellationToken::new());
graph.run_inline(&mut engine, 100).await?;
```

### 5.5 持久化执行与恢复

当前恢复使用 `Checkpoint` 保存状态投影、`next_node`、累计步数和图指纹；Agent 提供 `invoke_with_checkpoint` / `invoke_with_restore`。完成态恢复不执行节点，恢复后的保存继续使用原 trace。

原蓝图中的 `ExecutionSession` / `FrameStack` 设计已由[串行恢复设计](../discuss/lellm-restore-phase1-design.md)调整，不作为当前可用 API。新 `DurableRuntime` 在共享 Graph 执行路径上提供 start / inspect / submit_decision / resume / resolve_task / cancel，format=2 信封保存审批、任务、嵌套帧与预算；与旧入口并存。

## 六、关键设计决策

| 主题 | 说明 |
|------|------|
| Graph is Runtime | Graph 是唯一的 Runtime，Agent 是 DSL |
| Checkpoint Projection | `type Checkpoint` 关联类型，Runtime State 与 Checkpoint 分离 |
| Graph Hash | 新协议指纹包含起止节点、有语义的边顺序与嵌套图，另用显式 workflow/schema 版本约束业务兼容性 |
| 恢复位置 | 新协议保存每个 frame 的 next_node、steps_used、active 和状态投影；审批与任务身份包含路径和调用序号 |
| StateLens | 零拷贝状态投影，用于 Subgraph 组合 |
| Subgraph | Graph AST 的一种节点，Runtime 中递归执行 |
| Engine 借用 State | `ExecutionEngine<'a, S>` 持有 `&'a mut S`，调用方持有所有权 |

## 七、历史版本规划

本表记录原始版本意图，不代表这些版本或全部能力已交付。当前执行顺序与完成标准以本文顶部 M1–M4 及关联实施计划为准。

| 版本 | 范围 |
|------|------|
| **v0.1** | core + provider + agent + macros + MCP (Tools only) |
| **v0.2** | Graph/Node/Edge + 有环图 + BarrierNode + 流式执行 + 错误二分法 |
| **v0.3** | 6 crate 架构重构 + StateDelta + Checkpoint + ParallelNode + MCP |
| **v0.4** | ReAct = 有环图 + Typed State + Mutation 事件溯源 |
| **v0.5** | Graph is Runtime + Agent is DSL + Checkpoint Projection；ExecutionSession 旧方案已调整 |
| **v0.6** | 原计划包含 Multi-Agent / Durable Execution / Human-in-the-loop；现在按 M1–M4 分阶段验收 |
| **v0.7** | Sampling |
