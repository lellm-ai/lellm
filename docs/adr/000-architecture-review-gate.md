# ARG: MCP Architecture Review Gate

> **日期**: 2026-06-12
> **状态**: Passed（5/5 审查项通过，附修正）
> **审查者**: cunge
> **范围**: ADR-001 ~ ADR-004

## 审查结果

| # | 审查项 | 风险 | 结果 | 修正 |
|---|--------|------|------|------|
| R1 | Tool 生命周期 | **高** | ✅ 通过 | 引入 `ToolCatalog` 抽象，`ToolExecutor` 改数据源 |
| R2 | Transport 生命周期 | **中** | ✅ 通过 | 补充 `ConnState` 状态机，明确 fail-fast |
| R3 | 安全模型 | **中** | ✅ 通过 | 补 `TrustLevel` 到 ADR，审批在 Agent loop 层 |
| R4 | 协议边界 | **低** | ✅ 通过 | 确认冻结规则，严禁 MCP Agent |
| R5 | 观测面 | **低** | ✅ 通过 | 定义 Metrics + Tracing Targets |

## R1: Tool 生命周期（最高风险）

**问题**：`ToolExecutor` 假设静态注册（`Arc<HashMap>` clone 后不可变），但 MCP 工具天生动态。

**决策**：
- 引入 `ToolCatalog` trait（`definitions()` + `snapshot()`）
- `ToolExecutor` 改为持有 `Box<dyn ToolCatalog>`
- `StaticCatalog`（现有行为），`McpCatalog`（RwLock 保护），`CompositeCatalog`（组合）
- 刷新双模：Push（`tools/list_changed`）+ Pull（`client.refresh_tools()`）
- 工具消失时：正在执行的调用不受影响，后续查找返回 `NotFound`

**影响**：`ToolExecutor` 小改（数据源变更），执行逻辑不变。

## R2: Transport 生命周期

**问题**：Transport trait 缺少状态机，`request()` 在重连期间的行为未定义。

**决策**：
- `ConnState` 状态机由 `McpClient` 管理（6 状态：Disconnected/Connecting/Initializing/Ready/Broken/Closed）
- `request()` 在非 Ready 状态下 **Fail-fast**（返回 `Disconnected`，不阻塞）
- request-id 连续（monotonic counter，重连不重置）
- notification 不重放（重连后是全新连接）

## R3: 安全模型

**问题**：`TrustLevel` 未进入任何 ADR，审批机制可能侵入 `ToolExecutor`。

**决策**：
- `TrustLevel` 枚举：`Trusted` / `Confirm` / `Sandbox`
- 默认映射：本地=Trusted，stdio=Confirm，远程=Sandbox
- `ToolRegistration` 新增 `trust` 字段
- 审批在 **Agent loop** 层（hook），不侵入 `ToolExecutor`

## R4: 协议边界

**确认**：
- v0.3 仅支持：initialize, tools/list, tools/call, notifications
- 冻结规则：代码中不得出现 prompts/resources/sampling/roots method name
- 严禁 MCP Agent（避免与 Graph 打架）

## R5: 观测面

**决策**：
- `McpMetrics`（AtomicU64 计数器：connect/reconnect/tool_calls/errors/protocol_errors）
- Tracing targets：`mcp.transport`（trace）, `mcp.protocol`（debug）, `mcp.tool`（info）
- Agent Event 扩展：`McpConnected`, `McpDisconnected`, `McpReconnecting`, `McpToolRefreshed`

## 修正后的 ADR 清单

| ADR | 文件 | ARG 修正 |
|-----|------|---------|
| ADR-001 | `001-mcp-as-tool-runtime-extension.md` | +ToolCatalog, +ConnState, +TrustLevel, +观测面, +协议冻结规则 |
| ADR-002 | `002-mcp-crate-structure.md` | +ToolCatalog 归属, +依赖方向澄清 |
| ADR-003 | `003-mcp-transport-abstraction.md` | +ConnState 状态机, +fail-fast, +request-id 连续性, +notification 不重放 |
| ADR-004 | `004-mcp-tool-bridge.md` | +ToolCatalog 抽象, +TrustLevel 桥接, +双模刷新, +Human Approval |

## 进入 Phase B 的前提条件

- [x] 4 个 ADR 通过 ARG
- [x] ToolCatalog 抽象冻结
- [x] Transport 状态机冻结
- [x] TrustLevel 进入 ADR
- [x] 协议边界冻结
- [x] 观测面定义

全部通过，可以进入 Phase B（MVP 实现）。
