# ADR-002: lellm-mcp Crate 拆分策略

> **日期**: 2026-06-12
> **状态**: Accepted
> **决策者**: cunge
> **依赖**: [ADR-001](./001-mcp-as-tool-runtime-extension.md)

## 背景

MCP 需要新增代码，涉及 protocol models、transport 实现、client 逻辑、bridge 桥接。
如何拆分 crate 和 feature，决定后续依赖边界和维护成本。

## 决策

### 单一 crate，feature 控制

```
lellm-mcp/          ← 新增（协议 + client + bridge）
├── protocol/       # JSON-RPC 协议模型（无依赖）
├── transport/      # Transport trait + stdio impl
├── client/         # McpClient（连接管理、工具发现）
├── bridge/         # McpTool → ToolRegistration 桥接
└── server/         # (v0.4, feature-gated)
```

### 不拆 client/server crate

**不采用**：
```
lellm-mcp-client/   # ❌ 不拆
lellm-mcp-server/   # ❌ 不拆
```

**理由**：
- v0.3 只做 Client，Server 方案不成熟
- protocol 和 transport 是共享的，拆分导致重复
- 避免 feature 地狱（跨 crate 的 feature 组合爆炸）

### Feature 设计

```toml
[features]
default = ["stdio"]
stdio = []
sse = []          # v0.4
```

**不做** `client` / `server` feature——client 始终编译，server 等 v0.4 再决定。

### 依赖关系

```
lellm-agent
  ├── lellm-core
  └── defines ToolCatalog trait (不依赖 mcp)

lellm-mcp
  ├── lellm-core (protocol models 复用)
  ├── lellm-agent (bridge 依赖 ToolRegistration, McpCatalog 实现 ToolCatalog)
  ├── tokio (transport 异步 IO)
  ├── serde + serde_json (JSON-RPC)
  └── async-trait (Transport trait)
```

**依赖方向**：`lellm-agent` 定义 `ToolCatalog` trait → `lellm-mcp` 实现 `McpCatalog`。
**无循环**：agent 不依赖 mcp，mcp 依赖 agent 的 trait。

### ToolCatalog 归属

`ToolCatalog` trait 定义在 `lellm-agent`（因为 `ToolExecutor` 使用它）。
`StaticCatalog` 也在 `lellm-agent`（静态场景是基础功能）。
`McpCatalog` 在 `lellm-mcp`（实现 `ToolCatalog` trait）。
`CompositeCatalog` 在 `lellm-agent` 或 `lellm-mcp`（待定，倾向于 agent）。

## 后果

### 正面
- 单一 crate 降低维护成本
- Feature gate 控制编译体积
- protocol 模块可独立复用
- trait 依赖方向清晰（agent 定义，mcp 实现）

### 负面
- `lellm-mcp` 依赖 `lellm-agent`，引入 tokio 等运行时依赖
- 不使用 MCP 时，`lellm-mcp` 整个 crate 不被编译（通过 feature）
- `ToolExecutor` 需小改（`Arc<HashMap>` → `Box<dyn ToolCatalog>`）

### 风险
- 如果 `lellm-agent` 后续需要 MCP 能力，会产生循环依赖
- 缓解：bridge 层独立为 `lellm-mcp-bridge`（仅在必要时）
- `ToolCatalog` trait 边界可能随需求变化（如需要优先级、过滤等）
- 缓解：先实现核心接口（`definitions()` + `snapshot()`），后续扩展通过 trait default methods
