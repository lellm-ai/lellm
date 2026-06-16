# MCP 集成设计

> 来源：ADR-000~004 合并 | 日期：2026-06-12 | 状态：Accepted
>
> **一句话定位：** MCP = Remote Tool Runtime（远程工具运行时）。Agent 是中心，MCP 是扩展总线。

---

## 1. 架构审查（ARG）

ADR-000 审查结果（5/5 通过）：

| # | 审查项 | 风险 | 修正 |
|---|--------|------|------|
| R1 | Tool 生命周期 | **高** | 引入 `ToolCatalog` 抽象，`ToolExecutor` 改数据源 |
| R2 | Transport 生命周期 | **中** | 补充 `ConnState` 状态机，明确 fail-fast |
| R3 | 安全模型 | **中** | 补 `TrustLevel` 到 ADR，审批在 Agent loop 层 |
| R4 | 协议边界 | **低** | 确认冻结规则，严禁 MCP Agent |
| R5 | 观测面 | **低** | 定义 Metrics + Tracing Targets |

---

## 2. 定位与范围

### 核心定位

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

MCP 不参与编排，不创建新的 Agent 循环。

### 版本范围

| 能力 | v0.3 | v0.4 | v0.5 |
|------|------|------|------|
| Tools | ✅ | - | - |
| Resources | ❌ | ✅ | - |
| Prompts | ❌ | ✅ | - |
| Sampling | ❌ | ❌ | ✅ |

**冻结规则**：v0.3 代码中不得出现 `prompts/`、`resources/`、`sampling/` 的 method name。
**严禁**：MCP Agent（避免与 Graph 打架）。

### Client 优先

v0.3 只做 Client，v0.4 再做 Server。Client 立即可获得生态价值。

---

## 3. Crate 结构

```
lellm-mcp/
├── protocol/       # JSON-RPC 协议模型（无依赖）
├── transport/      # Transport trait + stdio impl
├── client/         # McpClient（连接管理、工具发现）
├── bridge/         # McpTool → ToolRegistration
└── server/         # (v0.4, feature-gated)
```

**Feature 设计**：
```toml
[features]
default = ["stdio"]
stdio = []
sse = []          # v0.4
```

**依赖关系**：
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

**ToolCatalog 归属**：`lellm-agent` 定义 trait，`lellm-mcp` 实现 `McpCatalog`。

---

## 4. Transport 抽象

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

**设计理由**：MCP 90% 是 request-response，notification 走独立流。单 `request()` 接口封装请求-响应语义，内部管理 request-id 匹配。

### 连接状态机

```
Disconnected → Connecting → Initializing → Ready → Broken
                                            ↓ (重连)    ↓ (不可恢复)
                                         Connecting   Closed
```

- 状态由 `McpClient` 管理，Transport 不感知
- `request()` 在非 Ready 状态下 **Fail-fast**（返回 `McpError::Disconnected`）
- request-id 连续（monotonic counter，重连不重置）
- notification 不重放（重连后是全新连接）

### 重连策略

- 指数退避：100ms → 200ms → 400ms → ... → max 5s，最大 5 次
- 可重试错误：`Disconnected`, `Timeout`
- 不可重试错误：`Protocol`, `InvalidParams`
- 重连后需重新 `initialize`（MCP 协议层逻辑）

### StdioTransport

```rust
pub struct StdioConfig {
    pub command: String,             // 如 "npx"
    pub args: Vec<String>,           // 如 ["@modelcontextprotocol/server-filesystem", "/path"]
    pub env: Option<HashMap<String, String>>,
    pub startup_timeout: Duration,
}
```

- 封装子进程生命周期
- read_loop 异步分发 response/notification
- notification 有界 channel（64），满时丢弃最新

### McpError

```rust
pub enum McpError {
    Disconnected,
    Timeout,
    Protocol(String),
    InvalidParams(String),
    ServerError(String),
    MethodNotFound(String),
    Io(io::Error),
}

impl McpError {
    pub fn is_retriable(&self) -> bool {
        matches!(self, McpError::Disconnected | McpError::Timeout)
    }
}
```

---

## 5. Tool 桥接

### ToolCatalog 抽象

```rust
pub trait ToolCatalog: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn snapshot(&self) -> Arc<HashMap<String, ToolRegistration>>;
}

pub struct StaticCatalog { ... }       // 现有行为
pub struct McpCatalog { ... }          // MCP 动态目录（RwLock 保护）
pub struct CompositeCatalog { ... }    // 静态 + 动态组合
```

`ToolExecutor` 改为持有 `Box<dyn ToolCatalog>`，每次查询最新快照。

### McpTool 桥接

```rust
pub struct McpTool {
    server: Arc<McpClient>,
    definition: ToolDefinition,
    tool_name: String,
}

impl McpTool {
    pub fn into_registration(self, safety: Option<ParallelSafety>, trust: TrustLevel) -> ToolRegistration {
        ToolRegistration {
            definition: self.definition,
            safety: safety.unwrap_or(ParallelSafety::Safe),
            category: Some("mcp".into()),
            func: Arc::new(move |args| {
                let server = self.server.clone();
                let tool_name = self.tool_name.clone();
                async move { server.call_tool(&tool_name, args).await }
            }),
            trust,
        }
    }
}
```

### 默认安全级别

远程工具默认 `ParallelSafety::Safe`（假设独立执行），可通过 `server.policy()` 覆盖。

### 刷新机制（双模）

- **Push** — Server 发 `notifications/tools/list_changed` → 自动刷新
- **Pull** — 用户显式调用 `client.refresh_tools()`

### 错误映射

| McpError | ToolErrorKind |
|----------|---------------|
| `Timeout` | `Timeout` |
| `Disconnected` | `Network` |
| `Protocol` | `InvalidInput` |
| Server 报错 | `Internal` |

---

## 6. 安全模型

### TrustLevel

```rust
pub enum TrustLevel {
    Trusted,        // 完全信任，无需确认
    Confirm,        // 首次调用需用户确认
    Sandbox,        // 沙箱执行（v0.4+）
}
```

| 来源 | 默认 TrustLevel |
|------|----------------|
| 本地函数 | Trusted |
| stdio MCP Server | Confirm |
| 远程 MCP Server | Sandbox |

### Human Approval

审批在 **Agent loop** 层，不侵入 `ToolExecutor`：
```
Agent Loop:
  1. 检查 trust.requires_approval()
  2. 如需确认 → 弹出 prompt → 用户 y/N
  3. 拒绝 → ToolError::PermissionDenied
  4. 通过 → 继续执行
```

---

## 7. 观测面

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

---

## 8. 关键设计决策

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
