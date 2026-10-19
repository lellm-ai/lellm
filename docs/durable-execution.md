# 可恢复执行：使用与迁移

更新：2026-10-19。M1–M4 首个完整切片，尚未发布新版本。

## 入口与执行边界

`lellm_graph::durable::DurableRuntime` 管理运行记录、独占权和决策，节点执行仍调用共享的 `Graph::run_inline`。`RunVersion::new(workflow, state_schema)` 必须使用非空、显式版本；修改闭包、Lens、合并策略、提示词或工具语义时升级 workflow，改变状态结构时升级 schema。结构指纹覆盖起止节点、边顺序、嵌套图和分支顺序，不能检测业务代码的全部变化。

```rust
use std::sync::Arc;
use lellm_graph::durable::{DurableRuntime, FileRunStore, RunVersion};

let runtime = DurableRuntime::new(
    Arc::new(FileRunStore::new("./runs")),
    RunVersion::new("order-review-v1", "order-state-v1"),
);
```

- `start(run_id, graph, state, max_steps, cancel)`：创建运行，已存在则拒绝。
- `inspect(run_id)`：读取最近完整记录，包括审批、任务收据和执行位置。
- `resume(run_id, graph, cancel)`：读取存储续跑；完成态直接返回，无节点执行。
- `submit_decision(...)`：绑定审批 ID、内容 token、客户端请求 ID 和 expected_revision；先保存再确认。
- `resolve_task(...)`：对账并提供原任务类型的结果，或显式授权重试；请求可去重。
- `cancel(run_id, expected_revision)`：取得独占权后保存终止状态。运行中返回 Busy，此时由拥有执行权的一方触发传入的 CancellationToken。

`RunOutcome` 分为 `Completed(state)`、`Suspended(record)`、`Stopped(status)`。执行错误返回 `Err`；它不表示外部操作已回滚。保存失败会禁止本次运行继续写入，重新取得租用后才能恢复。

可编译示例：[durable_approval.rs](../lellm-graph/examples/durable_approval.rs)：

```sh
cargo run -p lellm-graph --example durable_approval
```

## 审批

用声明式 `NodeKind::Barrier` 表示持久化审批；任意节点动态调用 `pause()` 不具备相同的恢复协议。审批 ID 包含图路径、节点和持久化调用序号；内容为到达审批时的状态投影，content_token 创建后固定不变。

`Approve` 允许继续；`Reject { reason }` 保存终止结果。决策接受与决策应用是两个保存边界，重启不会丢失已接受决策。重复的客户端请求返回原回执，即使提交的是旧 revision；冲突决策拒绝。Modify / Reroute 暂不属于新协议。

截止时间使用系统 UTC 的绝对毫秒数，恢复或提交时检查；过期终止，绝不隐式批准。暂停时无需常驻 Future，也不会继续占用文件锁。

Agent 使用 `AgentBuilder::require_tool_approval(true)`，再调用 `ToolUseLoop::invoke_durable` / `resume_durable`。审批位于模型生成工具参数之后、实际工具调用之前，恢复不重复已提交的模型节点。

## 工具收据与重试

`ExecutableTool::with_replay_policy(policy, version)` 显式声明恢复策略。`ParallelSafety` 仅描述并发安全，与重放安全相互独立。

| 策略 | Started 后没有已保存结果 | 工具内部重试 |
| --- | --- | --- |
| ManualRecovery（默认） | 暂停为 Unknown，人工对账或明确授权 Retry | 不自动重试 |
| ReplaySafe | 允许重新调用 | 使用现有 RetryPolicy |
| IdempotentWithKey | 使用原幂等键重新调用，外部系统负责去重 | 所有尝试传同一个键 |

IdempotentWithKey 必须配置 `with_context_executor`，否则执行前报错。上下文提供 task_id、idempotency_key、attempt 和取消查询。旧 `ToolFn`、宏工具和 MCP 适配继续可调用，默认 ManualRecovery；MCP 元数据不会被自动提升为幂等保证。工具默认版本 `unversioned` 仍依赖显式 workflow 版本管理。

顺序为 `Planned → Started → 外部调用 → Completed → ToolNode 消息提交`。每个工具独立保存结果；同一批次 A 完成、B 失败时，恢复复用 A，不重复追加消息或计入调用数。

| 崩溃窗口 | 恢复行为 |
| --- | --- |
| Planned 之前 / 之后、Started 之前 | 允许开始调用 |
| Started 已保存、尚未调用 | 按未知结果保守处理，无法区分是否已发出请求 |
| 外部已成功、Completed 尚未保存 | 根据三种策略重试或对账，不承诺 exactly-once |
| Completed 已保存、节点尚未提交 | 复用结果，重做纯后处理，然后提交节点 |
| 节点已提交 | 从后继位置继续 |

自定义节点可在执行期间取得 `TaskRuntime::current()` 并调用 `execute(slot, version, input, policy, closure)`；slot 在同一次节点调用中必须唯一。跨 spawn 必须显式传递 TaskRuntime，并等待子任务结束；不要将持有运行上下文的任务脱离执行生命周期。普通节点代码、模型请求、结果后处理仍可能重跑；有外部副作用的后处理需另包成任务。直接调用旧 `ToolExecutor` 单工具入口也不自动获得持久化收据。

## 子图、并行与预算

`SubgraphSpec` 通过原 StateLens 重建类型化子状态；自定义 StateProjector 默认拒绝 durable，必须提供能力声明和稳定指纹。子图完成结果先保存，父提交失败后复用该结果。

并行节点持久化各分支的完整状态。`ParallelNodeBuilder::graph_branch(name, graph, max_steps)` 可挂载真正的子图及其审批。成功分支不会因兄弟失败而重做；所有分支成功才按注册顺序合并，父状态和后继位置一起保存。合并函数必须确定、没有外部副作用。

- FailFast：首个失败到达时请求协作取消，并等待其他分支退出；不合作的分支会延迟返回，独占权继续保留。
- CollectAll：等待全部分支，返回每个失败的名称与错误；父状态不接收部分结果。
- 多审批：可以分别提交，但全部待决审批和 Unknown 任务处理完后才恢复执行。首版采用整个运行的就绪门槛。
- 总预算包含根图、子图、并行分支包装节点的逻辑调用，子图另有局部预算。恢复当前调用不重复扣减；循环再次进入会产生新身份并正常扣减。
- 取消不会撤销已经发生的外部副作用；已完成任务收据仍保留在记录中。

`with_observer` 接收携带 revision、frame、node、phase 的展示事件。子图与分支路径和存储一致；事件不是审计日志，不能依赖其恰好收到一次。回调应快速返回，异常不会修改控制协议。当前 durable 门面不提供模型 token 流，已有普通流式入口继续可用。

## 多 Agent 模板

`lellm_agent::templates::PlannerWorkersReviewer::new(planner, workers, reviewer, human_review)` 构建已有 Graph：

```text
Planner → prepare_workers → Workers（并行）→ prepare_review → Reviewer → 可选人工审批
```

使用 `team.initial_state(messages)` 初始化，传 `team.graph()` 给 DurableRuntime。每个角色都有独立 AgentState 和 system prompt；Planner 文本计划广播给固定 Worker，专业分工由其提示词定义。Worker 结果按注册顺序汇总，Reviewer 输出后可人工审批。返回 TeamState 可读取 planner / workers / reviewer。

这是固定拓扑模板，不把模型文本直接作为动态路由；增加模板种类、动态任务分配或 Supervisor 属于后续扩展。修改角色顺序、提示词和图配置时升级 workflow 版本。

## 兼容性与部署限制

1. 旧 format=1 Checkpoint / BlobCheckpointStore 和 `invoke_with_checkpoint` / `invoke_with_restore` 保留原串行恢复协议，仍拒绝 Barrier/Subgraph/Parallel。新入口使用独立 format=2 RunRecord。**不提供 v1 自动导入**：先用旧代码完成旧运行，新运行使用新入口和独立目录；业务状态迁移需显式设计。
2. 普通图没有 BarrierSink 时现在在执行前报错，修复原先静默批准。确认需要自动批准的顶层图应显式配置 `NoopBarrierSink`。普通 Subgraph 不传递审批处理器，含审批的组合请使用 durable；不能靠根处理器让子图隐式通过。
3. 文件后端仅支持本机文件系统，OS 独占锁覆盖一次执行或决策；不要在运行期间删除/替换 `run.lock`，也不要手工修改 run.json。读者通过原子替换看到完整信封。保证进程崩溃恢复，不承诺断电、NFS 或跨主机分布式租约。
4. 文件 IO 同步执行。标准库文件锁要求 Rust 1.89+；本项目本轮使用 1.98.1 验证。接口依据 [Rust File::try_lock](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)。
5. 首版每次保存完整运行信封，保留全部任务、审批和子帧，不提供自动裁剪。长循环会增加磁盘、序列化和克隆成本；先测实际负载，再设计增量存储和引用安全的回收。
6. 审批内容、消息和工具参数/结果进入存储。访问授权、加密、人工操作身份认证由宿主应用提供，content_token 仅用于绑定审批内容。

## 验证入口

- Graph：`approval_capabilities`、`durable_approval`、`durable_save_failure`、`durable_process`、`durable_tasks`、`durable_subgraphs`、`durable_parallel`。
- Agent：`checkpoint_restore` 中的 `durable_*` 测试覆盖审批、批次收据、幂等键和完整多 Agent 模板。
- 进程测试使用文件确认与 stdout 握手再 kill；含外部成功但收据未落盘的窗口，不依赖 sleep。

### 本轮验证结果（2026-10-19）

- `cargo test -p lellm-core -p lellm-graph -p lellm-agent --all-features --no-fail-fast`：通过。
- `cargo test --workspace --all-features --no-fail-fast`：472 通过、0 失败、31 忽略（原有忽略用例仍未执行）。
- `cargo fmt --all --check`、`git diff --check`：通过。
- 新增恢复测试使用 fake Provider 与工具，不要求真实模型服务。

本机执行 cargo 时移除了 DYLD_LIBRARY_PATH / DYLD_FALLBACK_LIBRARY_PATH，并限制 CARGO_BUILD_JOBS=2，避免环境动态库冲突；代码没有修改用户全局环境。
