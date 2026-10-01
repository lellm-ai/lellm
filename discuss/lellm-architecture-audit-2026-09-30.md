# LeLLM 全站架构审计报告

- **日期**：2026-09-30
- **性质**：讨论/评审文档（`discuss/`），非正式交付文档
- **状态**：分析完成；README 收紧 ✅、并行 delta 合并 ✅（P0-1）、HITL 拒绝/超时路由 ✅（P0-2，phase 1）、Google 工具往返 ✅（P0-3）、MCP 依赖瘦身 ✅（**P1 子项**，发布流程/CI 仍待办）、L1 AgentStateMerge base-delta 合并 ✅、R5 默认 feature 编译修复 ✅；持久恢复待单独排期

## 0. 审计基线（已锁定）

| 项 | 值 |
|---|---|
| 分支 / HEAD | `main` / `d488b35`（非浅克隆，= 公开 HEAD `github.com/lellm-ai/lellm`） |
| 远端 tag | **无**（`git ls-remote --tags` 退出码 0、零行） |
| 本地 tag | 无 |
| CHANGELOG | 无 |
| 未提交变更 | 2 文件（`calculator_graph_mock.rs`、`graph_test.rs`），均纯格式化，无语义改动 |
| cargo | 1.94.1；**stable 工具链本机损坏**（`dyld: missing symbol`），测试用 `cargo +1.88.0` / `RUSTUP_TOOLCHAIN=1.88.0` |
| 发布历史 | 只能靠 commit 考古（v0.2.0 → v0.4.11 约 12 个 bump commit，message 只有版本号） |

---

## 1. 执行摘要

本次全站分析确认 **4 个 P0 正确性问题** + **1 个 P1 发布/依赖问题**。头号发现：**README 宣传的「Durable Execution / Durable checkpointing」与实现严重不符**——恢复实际是「还原 state + 整图重跑」，不是「断点续跑」；主 Agent 路径根本没有 checkpoint。

**严重性排序（按实际影响，非结构坏味道）：**

| 级别 | 问题 | 影响 |
|---|---|---|
| **P0** | 并行合并静默丢数据 | 单分支改 base key 被另一分支的 base 值覆盖（已实证） |
| **P0** | HITL 拒绝/超时未正确应用 | Reject/Modify/Timeout 未被正确应用、仍走正常路由，可能继续执行受保护动作（Reroute/Cancelled 另有行为） |
| **P0** | Google 流式+工具往返实际坏 | 并行工具串号、tool-result 函数名 `"unknown"` |
| **P0** | Durable Execution 名不副实 | 恢复从头重跑、仅内存 store、主路径无 checkpoint（**另设里程碑**） |
| **P1** | 发布流程不保证发布包可用 + mcp feature 膨胀 | 无 test/`--no-verify`/`--allow-dirty`；mcp-only 用户编译 150 crate |

---

## 2. 全景对照表（文档承诺 → 代码实现 → 测试证据 → 缺口）

| 文档承诺 | 代码实现（file:line） | 测试证据 | 缺口 / 风险 |
|---|---|---|---|
| **Durable checkpointing / Durable Execution** | restore 只还原 state，从 `start_node` 整图重跑（`session.rs:210`+`graph_core.rs:346`）；`current_node` 无消费者（`checkpoint_data.rs:97`）；**游标语义矛盾**（存的是刚完成节点，注释写「下一个节点」）；save 是 `tokio::spawn` fire-and-forget（`execution_loop.rs:144`）；仅内存 store（`store.rs:81`）；**Agent 主路径 checkpoint=None**（`runtime.rs:136-143`） | 11 个 checkpoint 测试全过（`cargo +1.88.0`），但只覆盖存/取/hash 校验 | 崩溃后**重复执行副作用工具**；无磁盘 store；`MutationLog` 死代码；`TimeBased` 静默 no-op；**无 crash-recovery / 重复执行 / 新进程恢复测试** |
| **Human-in-the-loop** | 暂停/等待/re-wait 工作；但 `Approve\|Reject\|Modify` 同一 match 分支「继续正常路由」（`graph_core.rs:437-443`），`TimedOut` 注释称 Reject 却继续路由（`:444-446`）；`apply_decision_to_ctx` 死代码（`barrier_node.rs:80-107`） | 7 个 barrier 测试全过；`test_barrier_reject_with_back_jump`（`graph_test.rs:463`）弱断言掩盖死代码 | **Reject/Modify/Timeout 未正确应用、仍走正常路由，可能继续执行受保护动作**（Reroute/Cancelled 另有行为）；决策不改 state；崩溃后决策必丢；**无「拒绝后动作是否执行」测试** |
| **并行执行** | **全量 state 合并**（非 delta，`parallel_node.rs:218/235/259`）；同 key 按注册序 last-write-wins（`state_core.rs:117-125`）；`join_all` 等全部分支结束才处理 FailFast（`parallel_node.rs:266`） | 13 个 parallel 测试全过 | **单分支改 base key、另一分支不动 → 静默丢数据**（已实证）；`Reducer`/`StateConflict` 死代码；**FailFast 非执行层立即失败**；副作用不可回滚；**无 in-flight cancel / 单分支改 base key 测试** |
| **多 Provider 流式** | 共享 `ToolCallAccumulator`；但 **Google `index=0`/`id="unknown"`**（`google.rs:323/324`）；finish reason 值三家全丢；帧错静默吞（`stream_processor.rs:188-197`） | 39 内联 + 3 集成全过，但**跨 provider 流式工具拼接覆盖 = 0** | **Google 流式+工具往返实际坏**；Google image 能力声明是假的；`LlmError::Provider.code` 恒 None；**默认 feature 下集成测试编译失败** |
| **MCP client/server** | 有 36 个 protocol/server 测试，但无自动化流程验证完整功能；facade `mcp` feature 显式拉 `dep:lellm-agent`（`lellm/Cargo.toml:17`）→ **≈150 crate** | protocol/server 36 测试 | mcp-only 用户被迫编译 agent 运行时；死依赖（mcp `futures` 0 处 use、reqwest `blocking` src 0 处） |
| **发布** | `publish.sh` 存在；但**无 `cargo test`**、`--no-verify`、`--allow-dirty`、"已发布跳过"死代码、`rm -rf`+硬编码路径（`publish.sh:71-73`） | **无 CI、无 CHANGELOG、无 tag** | 无自动化流程验证发布包（无法排除历史人工验证）；部分失败后无法重跑；发布包与 commit 无法对应 |

---

## 3. 分类发现

> 按用户要求分三类：**已复现缺陷**（有最小复现）/ **源码确认的契约缺口**（读代码确认，未运行复现）/ **待验证风险**（需进一步验证）。

### 3.1 已复现缺陷（有最小复现，见 §7）

1. **并行合并静默丢数据**：base `count=0`，分支 A 改 `count=100`，分支 B 不动 → 合并结果 `count=0`（A 的写入丢失）。已在 `/tmp/merge_check` 用 `state_core.rs:117-125` 逐行复刻实证。**待转入仓库测试。**
2. **默认 feature 下 lellm-provider 集成测试编译失败**：`MockProvider` 被 `mock` feature gate（`lib.rs:16`）却在 `integration.rs` 无条件使用，`cargo test -p lellm-provider`（默认 feature）编译失败，必须 `--features mock`。✅ 已修复（commit `98eb55e`）：加自引用 `[dev-dependencies]` 启用 `mock`（测试构建 union 进 feature 集），对齐 workspace 既有模式，不影响下游 default feature。

### 3.2 源码确认的契约缺口（读代码确认）

1. **Durable Execution 游标语义矛盾**：`graph_core.rs:416` `emit_checkpoint(&current, step)` 在 execute+commit 之后调用，`current` 是**刚完成节点**；`execution_loop.rs:142` 把它存进 `Checkpoint.current_node`；但 `checkpoint_data.rs:96-97` 注释写「下一个要执行的节点」。**直接改恢复入口为 `current_node` 会重跑刚完成节点。**
2. **两套 checkpoint 对象**：`Checkpoint`（带 `current_node`，`checkpoint_data.rs:93`）与 `SessionCheckpoint`（带 `frames`，`session.rs`）恢复契约不统一；`ExecutionSession::restore`（`session.rs:198-216`）还原 state+frames 后 `run_inline` 恒从 `start_node` 起跑。
3. **HITL 部分决策未正确应用**：`graph_core.rs:437-446` 中 Reject/Modify/TimedOut 与 Approve 同走「继续正常路由」（未被正确应用），只有 Reroute/Cancelled 改变流向。**因此 Reject/Modify/Timeout 后可能继续执行受保护动作**（Reroute/Cancelled 行为不同，不在此列）。
4. **`apply_decision_to_ctx` 死代码**：`barrier_node.rs:80-107` 无调用方，Reject 不写 `reject_reason`，Modify 不应用修改。
5. **Google Provider**：`google.rs:323` `index=0`（并行串号）、`:324` `id=None`（finalize 成 `"unknown"`）、`:83-85` tool-result 回传 `name="unknown"`。
6. **发布流程**：`publish.sh:53-54` 无 test、`:106` `--no-verify --allow-dirty`、`:93-102` 死跳过检查、`:71-73` `rm -rf`+硬编码 `/Users/pengh/data/...`。

### 3.3 待验证风险（需进一步验证，不写成已确认缺陷）

1. **磁盘 checkpoint 是否真能避免工具重复执行**：工具已成功、checkpoint 未落盘时仍可能崩溃。**磁盘 checkpoint 单独不能保证 exactly-once**，需幂等键/结果去重/业务补偿。
2. **泛型 `WorkflowState` 的 delta 合并契约**：现有 `State` 可基于 base 做差异合并，但任意泛型 state 不能假定可自动比较，需明确 delta/merge 契约。
3. **mcp feature 收窄对既有用户的影响**：收窄现有 `mcp` feature 可能破坏既有依赖，需新增轻量入口 + 迁移路径。
4. **帧解析失败分类**：不能一律吞掉，也不能不分类就一律终止；需区分可忽略事件与损坏的关键数据。

---

## 4. 五个架构问题（位置 / 风险 / 测试证据 / 建议）

### 问题 1：Durable Execution 名不副实（P0，另设里程碑）
- **位置**：`session.rs:198-216`、`graph_core.rs:346/416`、`execution_loop.rs:142-159`、`runtime.rs:136-143`、`store.rs:81`、`checkpoint_data.rs:96-97`
- **风险**：README 核心卖点「Durable Execution」实际是「还原 state + 整图重跑 + best-effort 内存快照」。游标语义矛盾导致不能简单改恢复入口。主 Agent 路径无 checkpoint。
- **测试证据**：11 个测试全过但全是存/取/hash 单元链路，零 crash-recovery / 重复执行 / 新进程恢复覆盖。
- **建议**：见 §5 Q1（收紧范围的第一阶段）。

### 问题 2：HITL 拒绝/超时不阻止受保护动作（P0）
- **位置**：`graph_core.rs:437-446`、`barrier_node.rs:80-107`
- **风险**：Reject/Modify/Timeout 未被正确应用、与 Approve 同走正常路由，因此可能继续执行受保护动作（Reroute/Cancelled 另有行为）；`apply_decision_to_ctx` 死代码使 Reject 不写 state、Modify 不应用。
- **测试证据**：7 个 barrier 测试全过，但 reject_back_jump 弱断言掩盖死代码；无「拒绝后动作是否执行」测试。
- **建议**：见 §5 Q2（分阶段接通）。

### 问题 3：并行合并静默丢数据（P0）
- **位置**：`parallel_node.rs:218/235/259/266`、`state_core.rs:117-125`、`statekey.rs:15-32`
- **风险**：全量 state 合并导致单分支改 base key 被静默覆盖（已实证）。`Reducer`/`StateConflict` 死代码。`join_all` 等全部结束才处理 FailFast（**非执行层立即失败**）。
- **测试证据**：13 个测试全过，但同 key 测试只覆盖「两分支都写」，无「单分支改 base key、另一分支不动」场景。
- **建议**：见 §5 Q3（最小 delta + 冲突检测）。

### 问题 4：Google Provider 流式+工具往返实际坏（P0，对 Google 用户）
- **位置**：`google.rs:323/324/83-85`、`stream_processor.rs:188-197`
- **风险**：Google 并行工具串号、tool-result 函数名 `"unknown"`；finish reason 值三家全丢；帧错静默吞；Google image 能力声明是假的。
- **测试证据**：跨 provider 流式工具拼接零覆盖；默认 feature 集成测试编译失败。
- **建议**：小范围优先修（见 §6），含兼容策略。

### 问题 5：发布流程 + mcp feature 膨胀（P1）
- **位置**：`publish.sh:53-54/71-73/93-102/106`、`lellm/Cargo.toml:17`
- **风险**：仓库无自动化流程验证发布包独立构建（无法排除历史人工验证）；`--allow-dirty` 让发布包与 commit 无法对应；部分失败无法重跑；mcp-only 用户编译 150 crate。
- **测试证据**：无 CI、无 CHANGELOG、无 tag。
- **建议**：见 §6，含兼容策略。

---

## 5. 拟采用方案（待授权实施）

### Q1：Durable Execution —— 收紧范围的第一阶段（C 的限定版）

**不采用原版「current_node 续跑 + 磁盘存储」即视为可靠恢复**。先解决游标语义矛盾，限定为可验收的第一阶段：

1. **先支持明确范围内的串行图恢复**，明确循环、子图、并行、审批各自是否支持；**不支持的情况显式拒绝**（不静默降级）。
2. 保存「**已提交状态 + 准确的后续执行位置 + 必要控制状态**」。游标语义必须先统一：要么存「下一个要执行的节点」，要么存「刚完成节点 + resume-after 语义」，并修正 `checkpoint_data.rs:96-97` 的注释或取值。
3. **持久化成功后才越过对应执行边界**；保存失败向调用方返回（不再 fire-and-forget 静默丢）。
4. **用新进程加载检查点验证恢复**，不能只测内存对象存取。
5. **Agent 主入口若暂未接入，必须明确限制**（文档 + API 层面）。
6. **统一两条恢复路径**（`Checkpoint` vs `SessionCheckpoint`）的契约。

**明确不承诺**：磁盘 checkpoint 单独**不能**保证外部工具只执行一次（工具成功、checkpoint 未落盘时仍可能崩溃）。exactly-once 需幂等键/结果去重/业务补偿，**不写进第一阶段承诺**。

### Q2：HITL —— 分阶段接通（不机械连旧函数）

**报告「只有 Reroute 有效」过于绝对**：暂停、等待、批准后继续本身就是有效行为。真正要修的是：
- Reject 与 Approve 当前走相同正常路由；
- Modify 没有应用修改；
- 超时注释声称默认 Reject，执行却仍继续正常路由。

**尤其要检查：拒绝或超时之后，是否仍会执行需要批准的动作**（比「有没有写 reject_reason」更重要）。

分两步：
1. **先定义并验证单次运行中的行为**：批准继续、拒绝进入指定拒绝路径或结束、修改提交后再路由、超时执行明确策略。
2. **再接入持久恢复**：审批实例身份、决策、状态修改、后续位置保持一致。

「崩溃后重新审批」可以是明确策略，但**必须拒绝旧审批请求的迟到结果，且不能在重新审批前执行受保护动作**。**无需为暂时没有持久恢复就删除正常运行中的 HITL 能力。**

### Q3：并行合并 —— 最小 delta 合并 + 冲突检测（不采用原版 B）

**报告案例只有一个写入者（A 改、B 不动），不存在两分支写冲突**。只启用 `Reducer`/`StateConflict` 无法区分「B 没动」和「B 写了 0」。修复需知道**每个分支相对 base 改了什么**：

| 分支行为 | 合并预期 |
|---|---|
| A 修改，B 不动 | 保留 A 的修改 |
| A 删除，B 不动 | 保留删除 |
| A、B 修改不同字段 | 两者保留 |
| A、B 修改同一字段 | 按显式 reducer 合并，或报冲突 |
| A 删除，B 修改同一字段 | 按明确策略处理，默认报冲突 |

- 先针对现有 `State` 做**基于 base 的差异合并**，不必同时完成整套事件日志体系。
- 对任意泛型 `WorkflowState`，提供**明确的 delta/merge 契约**，不能假定所有状态都能自动比较。
- **补入契约检查**：当前 `join_all` 等所有分支结束再处理 FailFast，**目前不是执行层立即失败/取消**，需在契约中明确。

---

## 6. 明确缺陷（直接修，含兼容策略）

> 问题 4、5 不全是「无需取舍直接修」，以下事项需明确兼容策略。

### 6.1 Provider（问题 4）
- **小范围优先修**：Google `index`/`id`（`google.rs:323/324`）、能力声明与编码一致（Google image）。
- **帧解析失败**：不能一律吞掉，也不能不分类就一律终止；**区分可忽略事件与损坏的关键数据**（可忽略 → warn 跳过；关键数据损坏 → 发 `StreamEvent::Error`）。
- **finish reason 传递**：新增可能影响公共 API（`ProviderEvent` 无 FinishReason 变体），需评估 API 兼容。
- **默认 feature 编译修复**：`MockProvider` 的 feature gate 与测试用法对齐。✅ 已完成（commit `98eb55e`）。
- **补跨 provider 流式工具拼接一致性测试**。

### 6.2 发布 + 依赖（问题 5）
- **`publish.sh`**：加 `cargo test`；去 `--no-verify`/`--allow-dirty`；修「已发布跳过」死检查；去 `rm -rf`+硬编码路径（违反安全规则）；**验证打包产物**（`cargo package` + 独立构建）；处理 workspace 包发布顺序（不能只删两个参数）。
- **mcp feature**：收窄现有 `mcp` feature 可能破坏既有用户，**优先新增轻量入口（如 `mcp-stdio`）+ 迁移路径**，不直接收窄。
- **reqwest `blocking`**：删除前给仍使用它的 examples（4 处）保留所需配置。
- **死依赖**：删 lellm-mcp 的 `futures`（0 处 use）。
- **加 CI + CHANGELOG + git tag**。

---

## 7. 最小复现（待转入仓库测试）

> 记录命令、feature、工具链。**不要只保留 `/tmp` 中的验证。** 工具链统一 `cargo +1.88.0`（stable 损坏）。
> 状态分三类：**已运行复现**（已实际执行观测）/ **源码推断**（读代码确认，未运行）/ **待新增回归测试**（需调用仓库真实实现验证完整链路）。

| # | 复现 | 状态 | 命令 / feature / 工具链 | 预期 vs 实际 |
|---|---|---|---|---|
| R1 | 并行合并静默丢数据 | ✅ 已转入仓库回归测试（`parallel_test.rs` 5 个 delta 回归测试，16 全过） | `cargo +1.88.0 test -p lellm-graph --test parallel_test` | 预期 `count=100`，修复后实际 `count=100` |
| R2 | 拒绝/超时后受保护动作仍执行 | ✅ 已转入仓库回归测试（`graph_test.rs` 3 个 R2 回归测试，10 barrier 测试全过） | `cargo +1.88.0 test -p lellm-graph --test graph_test barrier` | 预期 protected 不执行，修复后实际不执行 |
| R3 | Google 流式 tool-result 函数名 `"unknown"` | ✅ 已转入仓库回归测试（`google.rs` 2 个 R3 测试 + `handle_frame` 并行 delta 测试，51 全过） | `cargo +1.88.0 test -p lellm-provider --features mock` | 预期函数名正确，修复后实际正确（id=函数名） |
| R4 | 恢复从头重跑（非断点续跑） | 源码推断 → 待新增回归测试（新进程加载） | `cargo +1.88.0 test -p lellm-graph` | 预期从断点续跑，源码推断从 start 重跑 |
| R5 | 默认 feature 集成测试编译失败 | ✅ 已修复（commit `98eb55e`，默认 feature 3 集成测试全过） | `cargo +1.88.0 test -p lellm-provider`（默认 feature） | 预期编译通过，修复后实际编译通过 + 3 测试全过 |

---

## 8. 行动清单（优先级）

**前置（验证完成前）**：
- [ ] **收紧 README** 中超出实现能力的承诺（Durable Execution、HITL 审批语义）

**P0（优先处理）**：
- [x] 并行数据丢失（Q3 最小 delta + 冲突检测）+ R1 测试 ✅ 2026-09-30（commit `ee16f2b`）
- [x] 审批拒绝/超时路径（Q2 阶段 1：单次运行行为）+ R2 测试 ✅ 2026-09-30（commit `bdd973a`；state 记录 / Modify 应用推迟 phase 1b，需泛型缝决策）
- [x] Google 工具往返（§6.1 小范围修）+ R3 测试 ✅ 2026-09-30（commit `a24c0bb`；**附带修复** `stream_processor.rs` FrameResult 并行 delta 丢失 bug——原 `tool_call_delta` 单 `Option` 同帧互相覆盖，审计报告未列出，是"并行串号"根因之一）

**里程碑（明确边界，单独排期）**：
- [ ] 恢复能力第一阶段（Q1 限定版）+ R4 新进程恢复测试

**P1（MCP 依赖瘦身子项已完成；发布流程/CI 仍待办，整个 P1 未完结）**：
- [ ] 发布流程（§6.2）+ CI/CHANGELOG/tag
- [x] **MCP 依赖瘦身** ✅ 2026-10-01（commit `babf271`）：新增 facade `mcp-stdio`（仅 stdio，不拉 agent/provider/reqwest/hyper/TLS）；根 workspace `lellm-mcp` 改 `default-features = false`，agent 侧显式 `default-features = true` 保行为；删 `lellm-mcp` 死依赖 `futures`（0 处 use，sse 验证通过）；`lellm::mcp` 导出开放给 `mcp-stdio`。独立消费项目实测：mcp-stdio 仅 ~52 crate（原 mcp 181），依赖边界干净（无 agent/provider/reqwest/hyper/TLS）
  - **编译 + 语义均已修复**：P0-1 遗留的 `AgentStateMerge::merge` 签名未对齐 base-based trait 已加 `_base` 参数对齐；**合并语义**已按 `StateMerge` 的 base-delta 契约重写（commit `4b25670`）——见下方 L1

**已解决（原遗留问题 L1）**：
- [x] **L1：`AgentStateMerge::merge` 合并语义错误** ✅ 2026-10-01（commit `4b25670`）。原为朴素全量合并（`messages.extend` 重复公共历史、计数器 `max` 无法 reset、`stop_reason`/`last_response` 可能拼出从未出现的终态），无 base 语义。已按 `StateMerge` 的 base-delta + 冲突检测契约重写（`typed_state_merge.rs`）：
  - `messages`：追加后缀按注册顺序拼接（`base + suffixes`，无重复）；replace 与非空追加并存或双 replace → 冲突；单 replace 其余未动 → 取该 replace。
  - `iterations`：全未变→base；单分支变→取该值；多分支全≥base→max；任一<base→冲突。
  - 累加器（`total_tool_calls`/`output_tokens`/`reasoning_tokens`/`compact_count`）：全未变→base；单分支变（含清零）→取该值；多分支全≥base→`base + Σ增量`（checked，溢出报错不回绕）；任一<base→冲突。
  - `stop_reason` + `last_response` 成组（stop_reason 由 last_response 派生）：单写者保留，多写者冲突（不拼接从未出现的终态）。
  - 空分支→base；单分支→该分支。
  - 配套：lellm-core 给 `Message`/`ChatResponse`/`TokenUsage` 补 `PartialEq`（前缀比较与 last_response 相等所需）；新增 `tests/agent_state_merge.rs`（18 用例钉死上述语义）。
  - **仍为潜在路径**：ReAct 图纯串行，`merge` 运行时仍未被调用；但语义已正确且有测试覆盖，agent 图引入 ParallelNode 时合并已就绪。

**前置条件（修改对应接口/传播前必须先定义，不可先改后定义行为）**：
- [ ] 泛型 `WorkflowState` 的 delta/merge 契约（**并行修复的前置**：改接口前必须明确，不能假定所有 state 可自动比较）
- [ ] 帧解析失败分类策略（**错误传播修改的前置**：区分可忽略事件 vs 关键数据损坏）

**暂缓（不阻塞上述，扩大功能时再做）**：
- [ ] exactly-once 工具执行（幂等键方案）

---

## 9. 记忆更新待办（结论确认后执行）

- 更新 `v04-progress-status`：v0.5 实际状态（Durable Execution 名不副实、HITL 审批空转、并行合并丢数据）
- 新增：游标语义矛盾（`current_node` 存刚完成节点 vs 注释「下一个节点」）
- 新增：HITL 决策不影响路由（`graph_core.rs:437-446`）
- 修正：`checkpoint-three-layer-architecture` / `barrier-rewait-semantics` 中过期术语（`DecisionRegistry`/`wait_barrier_decision` 不存在，当前是 `ChannelBarrierSink`）
