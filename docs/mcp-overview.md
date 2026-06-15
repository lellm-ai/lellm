# MCP 集成概览

> **状态**: Phase A 完成（架构冻结，ARG 5/5 通过）
> **目标版本**: v0.3
> **最后更新**: 2026-06-12

## 一句话定位

**MCP = Remote Tool Runtime（远程工具运行时）**

Agent 是中心，MCP 是扩展总线。MCP 不参与编排，不创建新的 Agent 循环。

## 架构总览

```
Graph (v0.2)
 ↓
Agent (ToolUseLoop)
 ↓
ToolExecutor ← ToolCatalog (新增抽象)
 ↓
ToolRegistration
 ├─ LocalTool (现有)
 └─ McpToolBridge (v0.3)
      ↓
   McpClient (状态机 + 重连 + 心跳)
      ↓
   McpTransport (stdio)
      ↓
   外部 MCP Server
```

## 快速导航

| 文档 | 内容 | 读者 |
|------|------|------|
| [ARG 评审报告](./adr/000-architecture-review-gate.md) | 5 项架构审查结果 + 修正 | 架构师、Tech Lead |
| [ADR-001 职责边界](./adr/001-mcp-as-tool-runtime-extension.md) | MCP 定位、版本路线、安全模型、观测面 | 全体 |
| [ADR-002 Crate 拆分](./adr/002-mcp-crate-structure.md) | 单一 crate 策略、ToolCatalog 归属 | 后端开发 |
| [ADR-003 Transport 抽象](./adr/003-mcp-transport-abstraction.md) | Transport trait、6 大接口深挖 | 后端开发 |
| [ADR-004 Tool 桥接](./adr/004-mcp-tool-bridge.md) | ToolCatalog 抽象、McpTool 桥接、刷新机制 | 后端开发 |

## 冻结的接口

### Transport Trait

```rust
#[async_trait]
pub trait McpTransport: Send + Sync + 'static {
    type Stream: Stream<Item = JsonRpcNotification> + Send + 'static;

    async fn connect(&self) -> Result<(), McpError>;
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<JsonRpcResponse, McpError>;
    fn notifications(&self) -> Self::Stream;
    async fn close(&self) -> Result<(), McpError>;
}
```

### ToolCatalog Trait

```rust
pub trait ToolCatalog: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn snapshot(&self) -> Arc<HashMap<String, ToolRegistration>>;
}
```

实现：`StaticCatalog`（agent crate）, `McpCatalog`（mcp crate）, `CompositeCatalog`

### 连接状态机

```
Disconnected → Connecting → Initializing → Ready → Broken
                                            ↓ (重连)    ↓ (不可恢复)
                                         Connecting   Closed
```

### 安全模型

| 来源 | 默认 TrustLevel |
|------|----------------|
| 本地函数 | Trusted |
| stdio MCP Server | Confirm（首次调用需确认） |
| 远程 MCP Server | Sandbox（v0.4+） |

审批在 **Agent loop** 层，不侵入 `ToolExecutor`。

## v0.3 范围

### 包含

- ✅ MCP Client
- ✅ Tools（initialize → tools/list → tools/call → notifications）
- ✅ stdio Transport
- ✅ ToolCatalog 抽象
- ✅ ToolBridge（McpTool → ToolRegistration）
- ✅ 连接状态机 + 重连
- ✅ TrustLevel 安全模型
- ✅ 观测面（Metrics + Tracing）

### 不包含

- ❌ Resources
- ❌ Prompts
- ❌ Sampling
- ❌ Roots
- ❌ Logging
- ❌ MCP Agent（严禁，与 Graph 冲突）
- ❌ MCP Server（v0.4）

## 版本路线

| 版本 | 范围 |
|------|------|
| v0.3 | MCP Client (Tools, stdio, ToolBridge) |
| v0.4 | MCP Server + Resources + HTTP/SSE Transport |
| v0.5 | Sampling + Agent↔Agent via MCP |

## 关键设计决策

| 决策 | 理由 |
|------|------|
| 桥接到 ToolRegistration，不改执行逻辑 | 最小抽象增量 |
| ToolCatalog 抽象动态工具 | MCP 工具天生动态 |
| 远程工具默认 Safe | 大多数 MCP Server 自身管理并发 |
| Transport 不感知状态机 | 保持传输层纯粹性 |
| request() 非 Ready 时 fail-fast | 不阻塞，让 RetryPolicy 决定 |
| 重连在 McpClient 层 | 重连后需重新 initialize（协议层逻辑） |
| 心跳在 McpClient 层 | ping 是 MCP 协议方法 |
| 单连接，不做池 | 简单，后续可扩展 |
| 审批在 Agent loop | 不侵入 ToolExecutor |

## 依赖关系

```
lellm-agent
  ├── lellm-core
  └── defines ToolCatalog trait

lellm-mcp
  ├── lellm-core (protocol models)
  ├── lellm-agent (bridge 依赖 ToolRegistration, McpCatalog 实现 ToolCatalog)
  ├── tokio (transport 异步 IO)
  ├── serde + serde_json (JSON-RPC)
  └── async-trait (Transport trait)
```

无循环依赖：agent 定义 trait，mcp 实现 trait。

## 观测面

### Metrics

```
connect_count, reconnect_count, tool_calls, tool_errors, protocol_errors
```

### Tracing Targets

```
mcp.transport   — Transport 层（IO 细节，trace level）
mcp.protocol    — 协议层（request/response，debug level）
mcp.tool        — 工具层（调用/结果摘要，info level）
```

### Agent Event 扩展

```
McpConnected, McpDisconnected, McpReconnecting, McpToolRefreshed
```

## 下一步

- [ ] Phase B: MVP 原型（stdio + initialize + tools/list + tools/call）
- [ ] 跑通 `npx @modelcontextprotocol/server-filesystem`
- [ ] Phase C: `/gsd-plan-phase` 拆解 v0.3 实现任务
