# ADR-001: MCP 定位为 Tool Runtime Extension

> **日期**: 2026-06-12
> **状态**: Accepted
> **决策者**: cunge
> **影响范围**: v0.3-v0.5 路线图, lellm-mcp crate, lellm-agent ToolExecutor

## 背景与问题

LeLLM 已有完整的本地 Tool 系统（定义 → 注册 → LLM 调用 → 结果回传）。
MCP 生态成熟，大量工具以 MCP Server 形式存在（filesystem, github, browser, postgres 等）。
需要将远程 MCP 能力无缝接入现有体系，同时避免 MCP 成为第二套编排系统。

## 决策

### 核心定位

**MCP = Remote Tool Runtime（远程工具运行时）**

Agent 是中心，MCP 是扩展总线。

```
Graph (v0.2)
 ↓
Agent (ToolUseLoop)
 ↓
ToolExecutor
 ↓
ToolRegistration
 ├─ LocalTool (现有)
 └─ McpToolBridge (v0.3)
      ↓
   McpClient
      ↓
   McpTransport (stdio)
```

MCP 不参与编排，不创建新的 Agent 循环。

### MCP 规范覆盖范围

| 能力 | v0.3 | v0.4 | v0.5 |
|------|------|------|------|
| Tools | ✅ | - | - |
| Resources | ❌ | ✅ | - |
| Prompts | ❌ | ✅ | - |
| Sampling | ❌ | ❌ | ✅ |
| Logging | ❌ | ❌ | later |
| Roots | ❌ | ❌ | later |

v0.3 仅实现：`initialize` → `tools/list` → `tools/call` → notifications

**冻结规则**：v0.3 代码中不得出现 `prompts/`、`resources/`、`sampling/` 的 method name（protocol model 中的 enum 变体除外，仅用于解析 Server 返回的 capability）。

**严禁**：MCP Agent（避免与 Graph 打架）。

### Client 优先

v0.3 只做 Client，v0.4 再做 Server。

**理由**：
- Client 立即可获得生态价值（接入现有 MCP Server 生态）
- Server 没有网络效应，延迟到 v0.4

### Crate 结构

```
lellm-mcp/
├── protocol/      # JSON-RPC protocol models
├── transport/     # Transport trait + stdio impl
├── client/        # MCP Client
├── bridge/        # McpTool → ToolRegistration
└── server/        # (v0.4, feature-gated)
```

Feature 控制（不拆 crate）：
- 默认：`["stdio"]`
- 可选：`sse`（v0.4）
- Server 代码随 crate 发布，但默认 feature 不编译

### 桥接设计

MCP 工具映射为 `ToolRegistration`：

```rust
pub struct McpTool {
    server: Arc<McpClient>,
    definition: ToolDefinition,
}

// 桥接为 ToolRegistration
ToolRegistration {
    definition: remote_tool.into(),
    safety: ParallelSafety::Safe,      // 默认 Safe（可通过 server.policy() 覆盖）
    category: Some("mcp"),
    func: Arc::new(move |args| {
        server.call_tool(tool_name, args)
    }),
}
```

**不改 `ToolExecutor`**。桥接层对 Executor 透明。

**安全策略覆盖**：
```rust
server.policy()
    .category_exclusive("filesystem")  // 同类串行
    .exclusive("dangerous_tool");      // 全局串行
```

### 动态工具发现 — ToolCatalog 抽象

**问题**：MCP 工具是动态的（Server 可能增删工具），但现有 `ToolExecutor` 假设静态注册（`Arc<HashMap>` clone 后不可变）。

**解决**：引入 `ToolCatalog` trait 抽象工具来源，`ToolExecutor` 改为每次查询最新快照。

```rust
pub trait ToolCatalog: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn snapshot(&self) -> Arc<HashMap<String, ToolRegistration>>;
}

pub struct StaticCatalog { ... }       // 现有行为
pub struct McpCatalog { ... }          // MCP 动态目录（RwLock 保护）
pub struct CompositeCatalog { ... }    // 静态 + 动态组合
```

`ToolExecutor` 改为持有 `Box<dyn ToolCatalog>`，`definitions()` 和 `execute()` 都通过 `catalog.snapshot()` 读最新数据。

**刷新模式（双模）**：
- **Push** — Server 发 `notifications/tools/list_changed` → Client 自动触发刷新
- **Pull** — 用户显式调用 `client.refresh_tools()`

**工具消失时的行为**：正在执行的调用不受影响（`ToolFn` 持有 `Arc<McpClient>`），后续查找返回 `ToolErrorKind::NotFound`。

### 传输层

抽象 `McpTransport` trait，v0.3 实现 stdio（详见 [ADR-003](./003-mcp-transport-abstraction.md)）：

```rust
#[async_trait]
pub trait McpTransport {
    type Stream: Stream<Item = JsonRpcNotification> + Send;

    async fn connect(&mut self) -> Result<(), McpError>;
    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;
    fn notifications(&self) -> Self::Stream;
}
```

设计理由：MCP 90% 是 request-response，notification 走独立流。

| Transport | v0.3 | v0.4 |
|-----------|------|------|
| stdio | ✅ | - |
| SSE | ❌ | ✅ |
| HTTP | ❌ | ✅ |

### 连接生命周期

单连接，不做连接池。

状态机：
```
Disconnected → Connecting → Initializing → Ready → Broken
                                                      ↓
                                                 Connecting (重连)
                                                    ↓
                                                  Closed (不可恢复)
```

**状态由 McpClient 管理**，Transport 不感知状态机。

重连策略：指数退避（100ms → 200ms → 400ms → ... → max 5s），最大 5 次。

**request() 在 non-Ready 状态下 Fail-fast**：直接返回 `McpError::Disconnected`，不阻塞。
理由：Agent 已有 RetryPolicy，阻塞会违反 ToolTimeout 契约。

### 安全模型 — TrustLevel

```rust
pub enum TrustLevel {
    Trusted,        // 完全信任，无需确认
    Confirm,        // 首次调用需用户确认
    Sandbox,        // 沙箱执行（v0.4+）
}
```

**默认映射**：

| 来源 | 默认 TrustLevel |
|------|----------------|
| 本地函数（现有） | Trusted |
| stdio MCP Server | Confirm |
| 远程 MCP Server（HTTP/SSE） | Sandbox |

**ToolRegistration 扩展**：
```rust
pub struct ToolRegistration {
    // ... existing fields ...
    pub trust: TrustLevel,
}
```

**审批不侵入 ToolExecutor**。在 Agent loop 中加 hook：
```
Agent Loop:
  1. 检查 trust.requires_approval()
  2. 如需确认 → 弹出 prompt → 用户 y/N
  3. 拒绝 → ToolError::PermissionDenied
  4. 通过 → 继续执行
```

限制：
- schema 大小上限
- tool 数量上限
- 调用 timeout
- output 长度上限

### 错误处理

```rust
enum RemoteError {
    Disconnected,   // → ToolErrorKind::Internal
    Protocol,       // → ToolErrorKind::InvalidInput
    Timeout,        // → ToolErrorKind::Timeout
    Server,         // → ToolErrorKind::Network
}
```

重试：本地工具默认不重试，远程工具允许重试。

### 流式工具输出

```rust
enum ToolOutput {
    Final(Value),
    Stream(Stream<Item = ToolChunk>),
}
```

MCP progress notification → `ToolDelta` 事件。

### 观测面

**Metrics**：
```rust
pub struct McpMetrics {
    pub connect_count:    Arc<AtomicU64>,
    pub reconnect_count:  Arc<AtomicU64>,
    pub tool_calls:       Arc<AtomicU64>,
    pub tool_errors:      Arc<AtomicU64>,
    pub protocol_errors:  Arc<AtomicU64>,
}
```

**Tracing Targets**：
```
mcp.transport  — Transport 层（IO 细节，trace level）
mcp.protocol   — 协议层（request/response，debug level）
mcp.tool       — 工具层（调用/结果摘要，info level）
```

**Agent Event 扩展**：
```rust
McpConnected { server }
McpDisconnected { server }
McpReconnecting { server, attempt }
McpToolRefreshed { server, tool_count }
```

## 后果

### 正面
- 最小抽象增量，复用现有 Tool/Agent/Runtime 全部能力
- Client 优先策略立即可用，零生态建设成本
- 桥接设计对 ToolExecutor 透明，不破坏现有代码
- 安全模型默认保守，避免盲目信任外部 Server

### 负面
- 远程工具默认 Safe（假设独立），若远端有共享状态需手动覆盖为 Exclusive/CategoryExclusive
- 单连接模型在高并发场景可能成为瓶颈（后续可加连接池）
- MCP 规范覆盖不完整，v0.3 不支持 Resources/Prompts
- `ToolCatalog` 抽象引入 `RwLock` 开销（微秒级，可忽略）和 trait 间接层

### 风险
- MCP 协议本身仍在演进，接口可能变化
- stdio Transport 对某些 Server 有启动延迟

## 未来演进

- v0.4: MCP Server + Resources + HTTP/SSE Transport
- v0.5: Sampling → Agent↔Agent via MCP（multi-agent 通信协议）
- `GraphNode::McpTool` — 允许 MCP 工具作为 Graph 节点
