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

### Changed
- **并行合并改为基于 base 的 delta 合并 + 冲突检测**（`lellm-graph`）：单分支改 base key 不再被其他分支的 base 值静默覆盖；并发写同一 key 返回 `MergeConflict` 而非 last-write-wins。
- **`AgentStateMerge` 改为 base-delta 合并**（`lellm-agent`）：`messages` 追加拼接 / replace 冲突、累加器 `base + Σ增量`（checked，溢出报错）、`stop_reason` + `last_response` 成组。
- **HITL Barrier 拒绝 / 超时路由修复**（`lellm-graph`）：`Reject` / `Timeout` 不再与 `Approve` 同走正常路由，受保护动作被正确阻止。
- **Google Provider 流式工具往返修复**（`lellm-provider`）：并行工具 `index` / `id` 串号、tool-result 函数名 `"unknown"`。

### Fixed
- 默认 feature 下 `lellm-provider` 集成测试编译失败（`MockProvider` 的 feature gate 与测试用法对齐：自引用 dev-dependency 启用 `mock`）。
- `stream_processor` 帧解析并行 delta 丢失（`tool_call_delta` 单 `Option` 同帧互相覆盖）。

### Compatibility
- ⚠️ **破坏性（API）**：`MergeStrategy::merge` 签名改为 base-based —— `fn merge(base: &S, branches: Vec<S>)`。实现该 trait 的类型需同步更新签名。
- **行为**：并行合并从 last-write-wins 改为 delta + 冲突检测（并发写同一 key 现在返回 `MergeConflict` 而非静默覆盖）。
- **兼容性（additive）**：`lellm-core` 的 `Message` / `ChatResponse` / `TokenUsage` 新增 `PartialEq`（纯新增，不破坏既有代码）。
