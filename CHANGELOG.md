# 变更日志（Changelog）

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式，
版本遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

> 说明：v0.4.11 之前无 CHANGELOG 记录（历史靠 commit 考古）。本文件从 v0.4.11
> 之后的**未发布源码变更**开始记录。`Unreleased` 的源码差异基线为 v0.4.11 的工作树
> （commit `d488b35`）；但**不**据此断言 v0.4.11 是已发布包的精确源码基线
> （远端无 tag、无已发布记录可对照）。

## [Unreleased]

### Added
- **`lellm::mcp` 新增 `mcp-stdio` 轻量入口**：仅 stdio 传输，不引入 agent/provider/reqwest/hyper/TLS。独立消费项目实测约 52 个 crate（原 `mcp` feature 约 181）。
  - 用法：`lellm = { version = "0.4", default-features = false, features = ["mcp-stdio"] }`
- **恢复能力第一阶段（Q1 限定版）**（`lellm-graph`）：串行图（含循环）进程崩溃后可从磁盘最新检查点恢复续跑（预算延续）。
  - `SimpleExecutor::execute_stream_with_checkpoint` — 持久化执行（每节点同步保存检查点，新 trace）
  - `SimpleExecutor::execute_stream_with_restore` — 持久化恢复（同一 trace 续跑；入口做结构 / 版本 / 指纹 / 步数 / **最新性**校验）
  - `FileBlobStore` 磁盘后端：trace 内单调数值 seq + flush/rename 原子可见，单写者约束
  - `Checkpoint` `format_version=1`：`next_node` 游标 + `steps_used` + sparkid `CheckpointId`
  - `CheckpointConfig::for_store` 便捷构造（store + codec）
  - `GraphEvent::CheckpointSaved` 事件（尽力而为观测）

### Changed
- **并行合并改为基于 base 的 delta 合并 + 冲突检测**（`lellm-graph`）：单分支改 base key 不再被其他分支的 base 值静默覆盖；并发写同一 key 返回 `MergeConflict` 而非 last-write-wins。
- **`AgentStateMerge` 改为 base-delta 合并**（`lellm-agent`）：`messages` 追加拼接 / replace 冲突、累加器 `base + Σ增量`（checked，溢出报错）、`stop_reason` + `last_response` 成组。
- **HITL Barrier 拒绝 / 超时路由修复**（`lellm-graph`）：`Reject` / `Timeout` 不再与 `Approve` 同走正常路由，受保护动作被正确阻止。
- **Google Provider 流式工具往返修复**（`lellm-provider`）：并行工具 `index` / `id` 串号、tool-result 函数名 `"unknown"`。
- **执行循环重排**（`lellm-graph`）：`run_inline_from` 统一入口 + 路由后**同步**保存检查点（移除 fire-and-forget 异步保存）；保存失败在边界停止并发 `GraphError`，不再产生「状态已前进但检查点缺失」的窗口。
- **Codec 两段式严格加载**（`lellm-graph`）：Value 解析 → 结构校验 → 类型化；legacy 格式 / 缺 `next_node` 显式返回 `UnsupportedFormat`，不再静默兼容。

### Fixed
- 默认 feature 下 `lellm-provider` 集成测试编译失败（`MockProvider` 的 feature gate 与测试用法对齐：自引用 dev-dependency 启用 `mock`）。
- `stream_processor` 帧解析并行 delta 丢失（`tool_call_delta` 单 `Option` 同帧互相覆盖）。
- **帧解析失败不再被静默吞掉**（`lellm-provider`）：`handle_frame` 现传播 codec 的解码错误，`process_stream` 对「无法安全继续处理的解码错误」（当前实例为 JSON 损坏）发 `StreamEvent::Error` 并中止流；可忽略帧（空帧 / 结束信号 / 未知事件）继续处理。注：此为**修复既有解码错误被吞掉**，非完整协议错误分类（合法但被当前实现忽略的 JSON 不必然语义可忽略）。

### Removed
- ⚠️ **破坏性（删除）**：旧 Session 系 checkpoint API — `ExecutionSession` / `SessionCheckpoint` / `SessionCheckpointSink` / `SessionError` / `Frame` / `FrameStack` / `MemorySink`（`lellm-graph`）。已被 `CheckpointSaveSink` + `execute_stream_with_checkpoint` / `execute_stream_with_restore` 架构取代。
- ⚠️ **破坏性（删除）**：`TriggerPolicy`（保存路径从不读取的死代码）、deprecated `CheckpointPolicy`、`RetentionPolicy::TimeBased`（静默 no-op）、`CheckpointConfig::{with_trigger, with_policy}`（`lellm-graph`）。第一阶段保存策略固定为「每节点同步保存 + 完成态保存」；旧配置含被删选项 → 编译期明确报错，不悄悄改默认值。

### Compatibility
- ⚠️ **破坏性（API）**：`MergeStrategy::merge` 签名改为 base-based —— `fn merge(base: &S, branches: Vec<S>)`。实现该 trait 的类型需同步更新签名。
- ⚠️ **破坏性（格式）**：Checkpoint 序列化格式升级为 `format_version=1`（`next_node` 游标 + `steps_used`）。旧格式检查点不可加载（显式 `UnsupportedFormat`，不静默兼容）。
- **行为**：并行合并从 last-write-wins 改为 delta + 冲突检测（并发写同一 key 现在返回 `MergeConflict` 而非静默覆盖）。
- **第一阶段边界**：恢复仅支持串行图（含循环）；Parallel/Subgraph/Barrier 图入口显式拒绝；不承诺外部副作用 exactly-once（工具成功但保存前崩溃 → 恢复重跑该节点，需幂等键）；进程崩溃安全 ≠ 断电安全（无 fsync）；同一 trace 单写者约束；恢复只接受该 trace 最新检查点；agent runtime 未接入（phase 2+）。
- **兼容性（additive）**：`lellm-core` 的 `Message` / `ChatResponse` / `TokenUsage` 新增 `PartialEq`（纯新增，不破坏既有代码）。
