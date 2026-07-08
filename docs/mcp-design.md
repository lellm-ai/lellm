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
    async fn connect(&mut self) -> Result<(), McpError>;
    async fn request(
        &self,
        req: JsonRpcRequest,
    ) -> Result<JsonRpcResponse, McpError>;
    fn notifications(&self) -> NotificationStream;
    async fn close(&mut self) -> Result<(), McpError>;
    fn state(&self) -> watch::Receiver<ConnectionState>;
}
```

**设计理由**：

- `connect()` / `close()` 使用 `&mut self` 表达**生命周期切换**——borrow checker 保证 connect 过程中不会并发调用 connect
- `request()` / `notifications()` / `state()` 使用 `&self`——JSON-RPC 天然支持并发请求
- Transport 只负责"发出去，等响应"——不关心 id 怎么来的、method 是什么

### McpClient — 协议层

```rust
pub struct McpClient {
    transport: Box<dyn McpTransport>,
    next_request_id: AtomicU64,          // 单调递增，重连不重置，Relaxed
    protocol_version: Arc<Mutex<Option<String>>>,
}

impl McpClient {
    /// 统一的请求入口 — 调用方只关心方法名、参数和返回类型。
    pub async fn request<P, R>(
        &self,
        method: &'static str,
        params: Option<P>,
    ) -> Result<R, McpError>
    where
        P: Serialize,
        R: DeserializeOwned;
}
```

**关键决策**：

| 决策 | 理由 |
|------|------|
| ID 由 McpClient 唯一生成 | Transport 不知道"第几个请求"，只负责传输 `request(id=N)` |
| `JsonRpcRequest::new()` 改为 `pub(crate)` | 业务代码永远不接触 id，杜绝 `id=0` bug |
| `request<R>(method, params)` 泛型返回 | 调用方直接拿到 `InitializeResult` / `CallToolResult`，不解析 `JsonRpcResponse` |
| `next_request_id` 用 `AtomicU64` + `Relaxed` | 只需要唯一性，不需要 happens-before 保证 |
| 重连不重置 ID | 日志可追踪（#381 → #382 → disconnect → #383），避免调试地狱 |
| initialize / tools_call / ping 走同一入口 | 消除特殊路径，统一 ID 分配 |

### 连接状态机

```
Disconnected → Connecting → Initializing → Ready → Broken
                                            ↓ (重连)    ↓ (不可恢复)
                                         Connecting   Closed
```

- 状态由 **Transport 主动驱动**（SSE stream 结束、子进程退出等），通过 `watch::Sender<ConnectionState>` 发射
- **McpClient 订阅** Transport 的状态变化，做 fail-fast 检查（`state.allows_request()`）
- `request()` 在非 Ready 状态下 **Fail-fast**（由 McpClient 层检查状态后返回 `McpError::Disconnected`）
- request-id 连续（monotonic counter，重连不重置）
- notification 不重放（重连后是全新连接）

### 重连策略

**核心原则：单一决策中心 — 恢复策略统一在 Agent/Runtime 层，McpClient 只提供原子能力。**

| 层次 | 职责 | 是否知道重连 |
|------|------|-------------|
| Transport | 建立/维持连接，报告状态 | ❌ 不决定 |
| McpClient | MCP 协议（initialize、request、pending） | ⚠️ 提供 `reconnect_once()` 能力，不做策略 |
| Agent / Runtime | 生命周期、恢复策略、重试、Fallback | ✅ 唯一决策中心 |

**为什么不在 McpClient 中自动重连：**

1. **双重 Retry 灾难** — Agent Retry(3次) × McpClient Retry(5次) × connect_timeout(30s) = 25 分钟
2. **Client 不知道全局状态** — Budget、Cancellation、Suspend、Checkpoint、备用 Tool……只有 Runtime 知道
3. **统一可预测** — HTTP Client、MCP Client、OpenAI Client、Redis Client……所有恢复策略由 Runtime 统一管理

**McpClient 提供的原子能力（无循环、无退避）：**
- `connect()` — 建立连接
- `reconnect_once()` — 单次重连（connect + initialize）
- `ensure_initialized()` — 检查是否已初始化，未初始化则初始化一次

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

## 5. Tool 桥接 — 五层架构 + 组合根

**核心原则：一个对象只有一种失效原因（Single Reason to Change）。**

### McpClient = Event Source

```
             McpClient  (Event Source)
            /    |    \
           /     |     \
   Registry   Catalog   Metrics / Tracing
           \     |     /
            \    |    /
         CompositeCatalog
                |
          ToolExecutor
```

**Registry 和 Catalog 是兄弟消费者，不是父子关系。** 它们共同依赖 `McpClient`，但互相不知道对方的存在。

### 组合根（Composition Root）

用户代码组装所有层，没有任何一层替另一层创建对象：

```rust
let registry = McpServerRegistry::new();
let client = registry.add_stdio(...).await?;  // Registry 提供 Client

let catalog = McpCatalog::new(client.clone()); // Catalog 消费 Client
composite.add_catalog(catalog);                // 组合层组装
```

### 各层职责

| 层 | 职责 | 不知道什么 |
|----|------|-----------|
| `McpTransport` | 传输（发出去，等响应） | id 来源、method 含义 |
| `McpClient` | 协议（ID 分配、initialize、request<R>、broadcast notifications） | 工具、Catalog、Registry |
| `McpServerRegistry` | 多 Server 生命周期（便利 API） | 工具、Catalog |
| `McpCatalog` | 发现（tools/list → ToolDefinition） | 闭包、Executor、Registry |
| `McpToolFactory` | 适配（ToolDefinition + Client → Tool） | Catalog 组合、Agent |
| `CompositeCatalog` | 组合（多个 Catalog → 一个快照） | MCP 细节 |

### 关键决策

| 决策 | 理由 |
|------|------|
| 删除 `McpMultiClient` | 四种失效原因混在一起 |
| McpClient 是 Event Source | Registry、Catalog、Metrics 都是兄弟消费者 |
| Registry 是便利 API | 不参与协议、Catalog 或 Tool 逻辑 |
| Catalog 绑定单个 Client | 发现是 1:1 关系，组合由 CompositeCatalog 处理 |
| Factory 独立 | Catalog 只做发现，不碰闭包 |
| Tool 闭包 discover 时构建一次 | 缓存为 `Arc<Tool>`，snapshot() 只 clone Arc |
| 路由由 Tool 自然完成 | 闭包已 capture `Arc<McpClient>`，不需要猜 JSON |

### Notification — broadcast 模型

```rust
impl McpClient {
    /// 订阅 notification — 任何人可以订阅，互不干扰。
    fn subscribe_notifications(&self) -> broadcast::Receiver<JsonRpcNotification>;
}
```

**为什么不用 `Stream`：** Stream 只能消费一次。`broadcast::Receiver` 允许多个订阅者（Catalog、Metrics、Tracing）。

### Runtime / Data 分离

**核心原则：数据对象不管理后台任务，所有后台任务由 Runtime 统一 spawn。**

```rust
// Data — 纯粹的缓存，零后台任务
pub struct McpCatalog {
    snapshot: ArcSwap<ToolSnapshot>,
}

// Runtime — 消费 notification，驱动 refresh
pub struct McpCatalogWatcher {
    client: Arc<McpClient>,
    catalog: Arc<McpCatalog>,
}

impl McpCatalogWatcher {
    /// 由 Runtime 统一 spawn，不自己 spawn。
    async fn run(self) {
        let mut rx = self.client.subscribe_notifications();
        loop {
            let notif = rx.recv().await;
            if notif.method == "notifications/tools/list_changed" {
                let new_snapshot = self.client.tools_list().await;
                self.catalog.store(Arc::new(new_snapshot));
            }
        }
    }
}
```

**为什么这样拆：**

| 问题 | Catalog 自己 spawn task | Watcher 独立 + Runtime spawn |
|------|------------------------|---------------------------|
| Drop 谁 abort | 需要 JoinHandle + CancellationToken 塞进 Catalog | Runtime 统一 Drop → abort |
| Arc 循环引用 | Catalog → Arc<Client> → task → Arc<Catalog> → Leak | 无循环，Runtime 持有 Watcher |
| 生命周期 | 隐藏在数据对象内部，不可见 | 显式在 Runtime 中管理 |
| 架构一致性 | 违背 Runtime/Data 分离 | 与 ExecutionSession/Checkpoint 风格一致 |

**完整生命周期：**
```
Application
     │
     ▼
Runtime
     │
     ├─ spawn CatalogWatcher.run()
     ├─ spawn HealthChecker.run()
     ├─ spawn Reconnector.run()
     ▼
McpClient
     │
     ▼
Transport

McpCatalog — 只是 ArcSwap<ToolSnapshot>，无后台任务
```

### ToolCatalog 抽象

```rust
pub trait ToolCatalog: Send + Sync {
    async fn snapshot(&self) -> Arc<ToolSnapshot>;
}
```

**McpCatalog 内部使用 `ArcSwap<ToolSnapshot>`** — Watcher refresh 时 store，外部 `snapshot()` 直接 load，零拷贝。

### 刷新机制（双模）

- **Push** — `notifications/tools/list_changed` → **McpCatalogWatcher** 收到 → 重新 `tools/list` → `ArcSwap::store(new_snapshot)`
- **Pull** — 用户显式调用 `catalog.refresh()`

### SSE POST URL — watch 替代 polling

```rust
// SSE reader 收到 endpoint 事件时：
post_url.send_replace(Some(full_url));

// connect() 等待处（零轮询）：
post_url.borrow().is_some() || post_url.changed().await;
```

**为什么用 `watch` 而非 `oneshot`：** reconnect 时需要等待新的 endpoint，`watch` 天然支持多次通知。

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
| Transport 驱动状态机，McpClient 订阅 | Transport 最了解连接生死（SSE断开/子进程退出），由它驱动状态最自然；McpClient 只观察并做 fail-fast |
| request() 非 Ready 时 fail-fast | 不阻塞，让 RetryPolicy 决定 |
| 重连策略在 Runtime 层 | 单一决策中心，避免双重 Retry 灾难；McpClient 只提供 reconnect_once() 原子能力 |
| 协议恢复在 McpClient 层 | connect + initialize + capability negotiation 属于 MCP 协议，封装为 reconnect_once() |
| 心跳在 McpClient 层 | ping 是 MCP 协议方法 |
| 单连接，不做池 | 简单，后续可扩展 |
| 审批在 Agent loop | 不侵入 ToolExecutor |
