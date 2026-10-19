# LangGraph 当前架构调研：供 LeLLM 蓝图评审使用

调研日期：2026-10-18。范围：Python LangGraph 官方文档、官方 GitHub 发布和固定版本源码；不以社区教程推断执行保证。本文前半部分是外部事实，设计建议明确标为推论。

对 LeLLM 的建议：保留现有分层与强类型状态，将下一阶段集中在审批、工具副作用、子图和并行的组合恢复。项目源码对照、验收顺序及待确认决策见本文后半部分。

> 2026-10-19 实施补充：下文项目对照保留调研时基线；本轮已交付能力与限制见 [可恢复执行](./durable-execution.md)。

## 版本与证据边界

本次直接查询 GitHub 官方 API，最新发布为 **LangGraph 1.2.16**，发布时间 **2026-10-16T22:11:28Z**，提交 `41c9c22`。搜索缓存最初只返回 1.2.13，因此版本判断以实时 API 与具体发布页为准。1.2.16 修复了恢复时重复应用 Command 更新、子图重复执行已完成任务、exit 模式重复保存 pending writes 等问题。这些发布说明也说明：嵌套恢复与提交边界需要专门的回归测试。[官方发布](https://github.com/langchain-ai/langgraph/releases/tag/1.2.16)、[官方最新版本 API](https://api.github.com/repos/langchain-ai/langgraph/releases/latest)

文档是滚动版本，下面的文档事实不等于每项 API 都首次出现在 1.2.16；涉及具体实现时使用固定标签源码。没有执行 LangGraph 与 LeLLM 的性能对比，本文不支持吞吐量倍数结论。

## 1. Agent 与 Runtime 的职责

LangGraph 官方定位是长时间、有状态执行的低层编排 Runtime；LangChain 提供模型、工具与 Agent 循环；Deep Agents 再提供规划、文件系统、子 Agent 等组合能力。使用 LangGraph 不要求采用 LangChain 的高层 Agent。[官方架构定位](https://docs.langchain.com/oss/python/langgraph/overview)

`create_agent` 返回编译后的图，因而可以作为另一张图的节点；嵌入后遵守子图执行与流输出规则。[Agent 作为子图](https://docs.langchain.com/oss/python/langgraph/streaming#subgraph-outputs)

**推论：**LeLLM 的 Provider / Graph / Agent 分层方向合理。“统一 Runtime”适合表达统一执行协议；它不要求全进程只有一个引擎对象。是否统一，应该由取消、持久化、审批和事件在嵌套调用中的行为验证。

## 2. 状态更新与并行提交

Pregel 将一次 superstep 分为计划、并行执行、更新三个阶段。执行阶段产生的 channel 更新直到下一步才对节点可见。`StateGraph` 和 Functional API 最终都使用 Pregel。[Runtime](https://docs.langchain.com/oss/python/langgraph/pregel)

Graph API 的节点返回部分更新，每个状态键有自己的 reducer；默认更新覆盖旧值。这里的协议不能描述成“节点直接修改共享 dict”。[State 与 reducers](https://docs.langchain.com/oss/python/langgraph/graph-api#reducers)

同一 superstep 中，多个节点写同一个未定义合并规则的键，会产生 `INVALID_CONCURRENT_GRAPH_UPDATE`；可以通过 reducer 明确如何合并。[并发更新错误](https://docs.langchain.com/oss/python/langgraph/errors/INVALID_CONCURRENT_GRAPH_UPDATE)

**推论：**Rust 的独占借用解决内存访问安全，无法独自决定失败回滚、分支提交和恢复重放。LeLLM 可以保留节点内部可变状态，但应明确哪些变更成为可提交的 `StateUpdate`，以及错误、取消、暂停时是否提交。全量克隆后做差异合并与返回增量，是需要基准测试与语义对照的两种实现选择。

## 3. Checkpoint、任务结果与持久化等级

完整 checkpoint 位于 superstep 边界；同一步内完成的任务另存 pending writes。某分支失败时，恢复可以复用其他已成功分支的结果。checkpointer 接口分别提供 `put` 与 `put_writes`。线程 checkpoint 与跨线程 Store 是不同用途。[Checkpointers](https://docs.langchain.com/oss/python/langgraph/checkpointers)、[Persistence](https://docs.langchain.com/oss/python/langgraph/persistence)

| 模式 | 提交时机与恢复边界 |
| --- | --- |
| `sync` | 开始下一步前等待 checkpoint 写入 |
| `async` | 下一步运行时异步写入；进程崩溃可能丢失尚未落盘的部分 |
| `exit` | 成功、错误或人工中断退出时保存；不提供执行中进程崩溃的中间进度恢复 |

来源：[Durability modes](https://docs.langchain.com/oss/python/langgraph/checkpointers#durability-modes)。

**推论：**只有“状态 + 下一个节点”的串行快照，不能直接扩展成并行恢复协议。需要同时表达调度位置、任务身份、已完成写入和提交状态。应先给出一种明确的持久化保证，再增加性能模式。

## 4. 恢复不是保存调用栈

`interrupt()` 恢复时从所在节点函数开头重新执行；同一节点内多次 interrupt 按调用位置匹配恢复值。子图作为函数被父节点调用时，父节点和发生中断的子节点都可能从头重跑。因此中断前的外部副作用必须可重入或幂等。[Interrupts](https://docs.langchain.com/oss/python/langgraph/interrupts)

节点内可以将操作封装为 task；已持久化的 task 结果在恢复时复用。改变恢复点之前的 task / interrupt 顺序可能使结果匹配错误。[节点重执行与 tasks](https://docs.langchain.com/oss/python/langgraph/graph-api#re-execution-and-idempotency)

**推论：**审批需要有明确的暂停记录、恢复输入和重复恢复策略。即使每步使用同步 checkpoint，“外部 API 成功、结果尚未落盘便崩溃”的窗口仍需要幂等键或外部事务协议；不能据此承诺外部副作用恰好执行一次。

## 5. 子图持久化与身份

子图的 `checkpointer=None` 表示默认每次调用独立、继承父级持久化；`True` 表示同一线程中保留子图记忆；`False` 禁用子图 checkpoint。前两种支持中断。每次调用隔离的 namespace 允许重复/并行调用；同一个有线程记忆的子图并行调用会争用 namespace。函数内子图调用顺序变化也可能改变状态归属；官方建议使用独立命名节点建立稳定 namespace。中断可以穿过嵌套传播到顶层，而状态检查另受子图是否可被静态发现的限制。[Subgraphs](https://docs.langchain.com/oss/python/langgraph/use-subgraphs)

**推论：**为 LeLLM 增加子图 checkpoint 时，应先确定 `(thread, graph path, invocation/task, checkpoint)` 身份，以及子图输入、父状态提交和生命周期规则。仅向子引擎转发一个 checkpointer 不足以提供这些保证。

## 6. 失败、取消与优雅停止

1.2.16 的 `_panic_or_proceed` 在发现未被处理的任务异常时，对其他尚未完成的 Future 调用取消，再传播异常；超时也取消剩余任务。这是取消请求，不能推导为撤销已发生的外部副作用，或强制杀掉运行中的同步代码。[固定版本 runner 源码](https://github.com/langchain-ai/langgraph/blob/1.2.16/libs/langgraph/langgraph/pregel/_runner.py#L657)

当前文档还提供 `RunControl.request_drain()`（要求 >=1.2）：等待当前 superstep 完成后保存可恢复 checkpoint，再停止；它明确不会取消正在运行的 asyncio task 或杀死线程。子图 drain 会向父级传播。[Graceful shutdown](https://docs.langchain.com/oss/python/langgraph/fault-tolerance#graceful-shutdown)

**推论：**LeLLM 应分别规定失败立即请求取消、等待当前步完成的 drain、人工暂停三种行为。`FailFast` 需要说明发现时机、兄弟任务处理和已完成结果是否保留。

## 7. 图迁移与业务版本

LangGraph 默认用当前图代码恢复历史线程，不自动把线程锁在启动时的代码版本。暂停线程依赖的节点重命名/删除可能破坏恢复；不兼容状态类型变化也有风险。官方建议在状态中记录业务版本，并按版本路由；旧节点/字段可经过兼容窗口再移除。[Backward compatibility](https://docs.langchain.com/oss/python/langgraph/backward-compatibility)

**推论：**结构 hash 是检测机制，不能证明行为兼容。LeLLM 应分别定义 checkpoint 格式版本、状态 schema 版本、业务工作流版本，以及“不兼容时拒绝、迁移或继续旧实现”的策略。

## 8. 事件输出与控制

当前官方推荐的进程内 event streaming 在 Pregel 原始事件之上建立 projections，可同时读取 messages、values、subgraphs、output 和 interrupts。协议事件包含单调序号与 namespace；transformer 负责观察并生成派生输出，不反向调用 Runtime。[Event streaming](https://docs.langchain.com/oss/python/langgraph/event-streaming)

**推论：**流消费者、执行控制和持久化应有各自的协议。LeLLM 应规定流接收端关闭时是继续执行、请求取消还是 drain；checkpoint、完成结果和人工审批不能只依赖一个可能被丢弃的展示事件。

## 9. 当前演进中值得关注的取舍

LangGraph >=1.2 的 `DeltaChannel` 仍标为 beta：将增量保存，读取时重建状态，可配置周期快照限制重放成本；reducer 要纯净并满足批次组合一致性，稳定 ID 应在写入前生成。[DeltaChannel](https://docs.langchain.com/oss/python/langgraph/pregel#deltachannel)

**推论：**这说明增量存储优化仍有格式、重放和迁移成本。LeLLM 更适合先验证恢复正确性，再根据真实消息长度、checkpoint 大小、存储时延决定是否引入 delta checkpoint。

## 未验证或不应推断的结论

- 未对 LangGraph、LeLLM 做同条件性能测试；语言选择不能直接证明端到端吞吐量。
- 本文没有把 Agent Server 的部署、断线行为等同于进程内 Runtime 保证。
- Future 取消不等于外部副作用回滚；pending writes 不等于跨系统 exactly-once。
- 滚动文档可能包含语言分支或新旧 API 混排；具体 Rust 接口设计应以执行契约和验收场景为准。


## 与 LeLLM 当前代码的对照

本节检查的是工作区基线提交 `35f392f` 的执行路径；工作区已有的 StateMerge 分配优化不影响以下结论。这里区分源码事实和架构建议，没有把蓝图中的完成标记当作实现证据。

| 领域 | 本项目源码事实 | 对蓝图的影响 |
| --- | --- | --- |
| Agent 与 Graph 分层 | `AgentBuilder::build()` 返回预构建的 `Arc<Graph<AgentState, AgentStateMerge>>`，`ToolUseLoop::invoke()` 调用图执行入口。[构建](../lellm-agent/src/runtime/builder.rs)、[调用](../lellm-agent/src/runtime/runtime.rs) | 保留 Agent DSL、Graph Runtime、Provider 的分层；当前方向与 LangChain/LangGraph 的职责划分相容。 |
| 串行恢复 | `Checkpoint` 保存投影状态、next_node、steps_used、graph_hash；每个成功节点同步等待保存，保存失败停止前进。[格式](../lellm-graph/src/checkpoint/checkpoint_data.rs)、[保存](../lellm-graph/src/exec/checkpoint_save_sink.rs) | 已有可用基础；这套格式描述一个串行执行位置，尚不足以单独描述多个分支或嵌套暂停。 |
| 可持久化范围 | `Graph::validate_persistable()` 明确拒绝 Parallel、Subgraph 和 Barrier。[校验](../lellm-graph/src/graph/graph_core.rs) | “可组合”“支持审批”“支持恢复”各自成立，并不意味着三者组合后成立。版本验收应覆盖组合场景。 |
| 子图能力继承 | 子图执行传递 stream、cancel，但 checkpoint、barrier 均为 None，步骤回调为 Noop；引擎在没有 BarrierSink 时返回 Approve。[子图](../lellm-graph/src/node/subgraph_spec.rs)、[引擎](../lellm-graph/src/exec/execution_engine.rs) | 已可从源码推导：嵌套审批会自动通过，子图没有父级检查点与步骤追踪。建议优先明确拒绝不支持的组合，再实现能力继承。此项未新增运行时复现测试。 |
| 并行与失败 | ParallelNode 克隆分支状态，用 `join_all` 等待全部分支，然后才判断 FailFast / CollectAll；成功分支通过 `MergeStrategy` 合并。[实现](../lellm-graph/src/node/parallel_node.rs) | 当前 FailFast 不会在首个错误到达时停止等待；CollectAll 最终也返回首个错误。先把名称、返回结果、取消行为与契约对齐，再增加分支恢复。 |
| 状态提交 | Mutation 由引擎提交，`WorkflowState` 提供 Checkpoint 投影，`MergeStrategy` 负责分支合并；Mutation trait 本身不强制序列化。[状态契约](../lellm-graph/src/state/workflow_state.rs) | 保留 Rust 强类型 Mutation。内存里的 mutation batch 不等于持久化日志，也不记录外部副作用的完成收据。 |
| 图版本兼容 | Graph 默认结构 hash 排序节点名和边，未包含起止节点、节点逻辑、条件内容；Agent hash 使用工具名称和部分配置。[图 hash](../lellm-graph/src/graph/graph_builder.rs)、[Agent hash](../lellm-agent/src/runtime/builder.rs) | 相同 hash 不能证明行为等价。例如改变多个同时成立条件边的顺序，可能改变 first-match 路由，却保留相同默认 hash。建议显式标识 workflow_version、state_schema_version、checkpoint_format_version，并约定迁移策略。 |

“唯一 Runtime”建议定义为统一的调度、提交、恢复与取消规则；递归中存在多个短生命周期 Engine 对象并不自动违反这个目标。真正需要追踪的是每项运行能力在子图边界是否被保留，以及恢复时能否重建执行位置。

## 其他官方方案提供的参照

Pydantic AI 的当前官方文档将持久化运行与保存聊天记录分开，并提供 Temporal、DBOS、Prefect 等后端集成；模型请求和工具调用可以成为独立的持久化单元。这说明 Agent SDK 可以复用专门的执行后端。对 LeLLM 的建议是继续保留轻量嵌入式运行方式，将外部执行后端作为未来接入方向，是否引入服务部署成本需另行决策。[Pydantic AI durable execution](https://pydantic.dev/docs/ai/capabilities/durable_execution/overview/)

Temporal 官方文档仍要求 Activity 具备幂等性：外部操作已成功但完成结果尚未记录时，重试可能再次执行它。对 LeLLM 的启示是，增加任务结果日志可以缩小重跑范围，但不会自动保证外部副作用 exactly-once；业务 API 的幂等键或对账机制仍然必要。[Temporal Activity idempotency](https://docs.temporal.io/activity-definition#idempotency)

## 蓝图顺序（优先级已确认）

后续用户已确认“审批可恢复 → 工具重放边界 → 子图和并行恢复 → 多 Agent 模板”的规划顺序。[实施计划](../discuss/lellm-runtime-reliability-plan.md)已拆分交付与验收，[蓝图](BLUEPRINT.md)已同步；以下架构细节仍是推荐方案。

建议产品定位为“可嵌入 Rust 应用的类型化 Agent 工作流运行时”。第一条完整用户场景应是：生成方案 → 人工审批 → 执行工具 → 输出结果；在审批期间及工具执行后的持久化边界注入进程崩溃，验证恢复行为。

以下按依赖排列，不直接绑定现有 v0.5/v0.6 标签，因为 README 和蓝图当前的版本状态并不一致。

1. **写清现有执行契约并补齐明显缺口。** 明确缺少审批处理器时的行为、并行错误策略、事件丢失是否允许、各入口取消规则。验收：顶层与子图审批均不能在未配置自动批准时静默继续；未支持的组合在执行前报错。
2. **让审批成为可持久化的暂停状态。** 保存等待的审批标识、输入、恢复位置与决策，而不是仅让进程内 Future 等待消息。验收：进程重启后仍等待同一审批；重复提交决策不会重复推进。
3. **明确工具级恢复边界。** 图节点可以包含多个 LLM/tool 操作；为需要重放保护的操作记录稳定任务身份与结果，并把同一幂等键传到重试和恢复后的工具调用。验收：已记录完成的操作复用结果；未记录完成的操作可能重试，行为必须符合声明的幂等或补偿约定。
4. **实现子图与并行的组合恢复。** 持久化子图 namespace / 执行位置，保存各分支已完成结果以及统一提交边界。验收：A 分支成功且已记录、B 分支失败时，恢复不会重做 A；子图审批、Trace 路径和预算能被父运行识别。
5. **在上述能力之上提供多 Agent 模板。** Supervisor / Planner 等应复用相同执行契约。Compiler InlinePass、分布式调度和可视化各自需要实际场景及基准，不作为当前恢复能力的前提。

建议继续使用同步检查点作为首个可持久化模式；等存储延迟成为已测量的瓶颈，再增加异步/退出时保存及其明确的数据损失窗口。Rust 的借用式 Engine 可以保留：持久化执行位置应存为可重建的数据，不试图保存 Rust 调用栈。是否采用显式 FrameStack 或按 namespace 分层保存，需要先定义嵌套恢复语义。

暂不建议照搬 LangGraph 整套 Pregel 调度器或 DeltaChannel。LeLLM 可以先实现显式 ParallelNode 的分支完成记录与提交规则，再由动态 fan-out、多节点同轮调度等需求决定是否引入 superstep。

## 需要纠正的旧对照材料

[原有 LangGraph 对比文档](langgraph-vs-lellm-graph.md)包含历史实现、概念简化和没有来源的性能结论，不能继续作为当前架构依据：

- LangGraph 节点按官方契约返回状态更新，由 reducer/channel 应用；“直接修改 dict”不足以描述其状态模型。
- LeLLM 当前 ParallelNode 使用并发轮询，旧文档写“当前顺序执行”已过时；并发轮询也不能据此声称 CPU 工作自动跨线程并行。
- AgentBuilder 当前构建预先生成的 Graph；旧文档中的 AgentFlowNode 三层封装不符合当前执行路径。
- “吞吐高几个数量级”需要一致负载、模型/工具延迟、并发度、存储模式和测试环境，现有文档没有这些证据。

本研究仅标出这些偏差，未重写既有 ADR 或把建议标记为已确认决定。

## 下一轮 grilling：先确定三个根决策

1. **产品验收主线：** 是否接受“嵌入式 Rust Agent 工作流，并优先完成审批与崩溃恢复的组合场景”？建议接受，再以此筛选新特性。
2. **恢复责任边界：** 框架是否负责任务身份、已完成结果复用与稳定幂等键传递，工具作者负责外部系统幂等/补偿？建议采用该边界，保留明确的重复执行窗口。
3. **现有 API 兼容性：** 为纠正静默审批通过、FailFast 名实不符等行为，是否允许在下一个明确版本中做有迁移说明的行为变更？建议允许，但先记录哪些现有入口依赖旧语义。

用户已确认恢复能力的优先顺序，并授权按此规划；工具责任边界和 API 行为兼容等细节尚未确认。后续关于运行身份、暂停与取消区别、并行提交顺序、子图 checkpoint 组织、状态迁移和后台运行生命周期的设计，继续按实施计划末尾的决策树评审。
