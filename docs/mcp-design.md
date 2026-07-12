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
| **整个系统只有 `McpClient` 持有 `next_id: AtomicU64`** | Transport 永远不生成/修改 Request ID，只负责传输已带 id 的 Request。这是唯一的 ID 来源 |
| **所有 `Transport` 只发送调用方提供的 `JsonRpcRequest`，绝不重新构造或修改其中的 `id`** | 保证 trait 语义一致——`request(JsonRpcRequest)` = "发送这个完整的请求" |
| `JsonRpcRequest::new()` 调用点收敛到 `McpClient` | 整个工程只有 `McpClient` 一处生成 Request，其他位置出现 `JsonRpcRequest::new()` 即表示有人越权 |
| `request<R>(method, params)` 泛型返回 | 调用方直接拿到 `InitializeResult` / `CallToolResult`，不解析 `JsonRpcResponse` |
| `next_request_id` 用 `AtomicU64` + `Relaxed` | 只需要唯一性，不需要 happens-before 保证 |
| 重连不重置 ID | 日志可追踪（#381 → #382 → disconnect → #383），避免调试地狱 |
| initialize / tools_call / ping 走同一入口 | 消除特殊路径，统一 ID 分配 |

**请求生命周期（职责链）：**

```
Caller
    │
    ▼
McpClient
    ├── next_id.fetch_add()
    ├── JsonRpcRequest { id, method, params }
    ▼
Transport
    ├── serde_json::to_string()
    ├── write_all()
    ├── pending.insert(id)
    ▼
Server
```

Transport 永远不知道：

- 当前是第几个请求
- ID 是否连续
- 是否重连
- 是否跨 Transport

它只负责：**发送一个已经带好 id 的 Request。**

### 连接状态机

```
Disconnected → Connecting → Initializing → Ready → Disconnected
                                            ↓ (主动关闭)
                                          Closed
```

**状态语义：**

| 状态           | 含义                                       | 是否允许 request |
| -------------- | ------------------------------------------ | ---------------- |
| Disconnected   | 当前没有可用连接（启动前、意外断开、重连前） | ❌                |
| Connecting     | Transport 正在建立连接                       | ❌                |
| Initializing   | MCP initialize 阶段                         | ❌                |
| Ready          | 可以发送请求                                 | ✅                |
| Closed         | 用户主动关闭，不会再重连                     | ❌                |

**关键决策（Grill 2026-07-12）：**

- **不引入 `Broken` 状态** — `Broken` 是瞬时错误事件，不是稳定状态。连接意外断开直接回到 `Disconnected`。错误原因通过 `McpError` 或日志/事件传递，不编码进 `ConnectionState`。
- **`Disconnected` 统一表示"当前没有可用连接"** — 无论原因是尚未连接、连接意外断开，还是等待下一次重连。
- **`Closed` 只表示主动关闭** — `client.close()` 或 Drop。与意外断开（`Disconnected`）严格区分。

- 状态由 **Transport 主动驱动**（SSE stream 结束、子进程退出等），通过 `watch::Sender<ConnectionState>` 发射
- **Transport 读取循环退出时，必须先发送 `Disconnected` 状态，再清理 pending requests** — 确保 `request()` 检查 `allows_request()` 时立即 fail-fast，不再向已死亡的连接写入数据
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
| `McpServerRegistry` | 多 Server 生命周期（连接、重连、Watcher、关闭） | 工具合并、Catalog |
| `RegistryCatalog` | 工具聚合（snapshot、merge、NameConflictPolicy） | 连接、Watcher |
| `McpCatalog` | 发现（tools/list → ToolDefinition） | 闭包、Executor、Registry |
| `CompositeCatalog` | 组合（多个 Catalog → 一个快照） | MCP 细节 |

**关键决策（Grill 2026-07-12）：**

- **`McpServerRegistry` 不实现 `ToolCatalog`** — Registry 只管 Server 生命周期，Catalog 只管工具聚合。变化原因完全不同。
- **`RegistryCatalog` 作为独立适配层** — 专门负责 merge/snapshot/conflict policy，实现 `ToolCatalog`。Registry 通过 `catalog()` 方法返回 `Arc<dyn ToolCatalog>`。
- **类比 `Router::service()` 而非 `impl Service for Router`** — Registry **可以生成 Catalog**，但 **Registry 不应该自己就是 Catalog**。
- 高级用户可以完全绕过 Registry，自行组合多个 `McpCatalog` 到 `CompositeCatalog`。

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

### 所有权模型 — Registry 统一管理

**核心原则：Registry 是所有后台任务的唯一 Owner，用户 API 只暴露 add_xxx()，无需关心 Watcher 生命周期。**

```rust
// 数据层 — 纯读接口，零后台任务
pub struct CatalogStore {
    snapshot: RwLock<Arc<ToolSnapshot>>,
}

pub struct McpCatalog {
    store: Arc<CatalogStore>,
}

// 写入层 — 唯一的刷新接口
pub struct CatalogRefresher {
    client: Arc<McpClient>,
    store: Arc<CatalogStore>,
}

// Watcher — 依赖 trait，不依赖具体类型（Command Pattern）
pub struct McpCatalogWatcher {
    refresher: Arc<dyn CatalogRefresh>,
    rx: broadcast::Receiver<JsonRpcNotification>,
}

// 受管理的服务器实例
struct ManagedServer {
    _client: Arc<McpClient>,
    store: Arc<CatalogStore>,
    cancel: CancellationToken,
    watcher: JoinHandle<()>,
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        self.cancel.cancel();  // 优雅退出
        if let Some(handle) = &self.watcher {
            handle.abort();    // 兜底保证退出
        }
    }
}

// Registry — 所有权的唯一 Owner
pub struct McpServerRegistry {
    servers: IndexMap<String, ManagedServer>,
}
```

**为什么这样拆：**

| 问题 | Catalog 自己 spawn task | Registry 统一管理 |
|------|------------------------|------------------|
| Drop 谁 abort | 需要 JoinHandle + CancellationToken 塞进 Catalog | ManagedServer::drop() → cancel + abort (RAII) |
| Arc 循环引用 | Catalog → Arc<Client> → task → Arc<Catalog> → Leak | 无循环，Registry 持有 ManagedServer |
| 生命周期 | 隐藏在数据对象内部，不可见 | 显式在 Registry 中管理 |
| API 复杂度 | 用户必须保存 JoinHandle | 用户只调用 add_xxx() |
| 职责分离 | Watcher 依赖 Client + Catalog | Watcher 只依赖 CatalogRefresh trait |

**为什么 `ManagedServer` 实现 `Drop`：**

- `CancellationToken` 被 drop 时**不会**自动 cancel — 只是丢弃引用
- Watcher task 会一直存活（`rx.recv()` 阻塞），泄漏 `Arc<McpClient>`、`Arc<CatalogStore>`
- `ManagedServer::drop()` 统一负责：`cancel.cancel()` + `watcher.abort()`
- 无论通过 `remove()`、`shutdown()` 还是 Registry 整体 drop，资源都会正确回收

**完整生命周期：**
```
Application
     │
     ▼
McpServerRegistry (唯一 Owner)
     │
     ├─ ManagedServer 1
     │    ├─ client: Arc<McpClient>
     │    ├─ store: Arc<CatalogStore>
     │    ├─ cancel: CancellationToken
     │    └─ watcher: JoinHandle
     │
     ├─ ManagedServer 2
     │    └─ ...
     │
     └─ Drop → ManagedServer::drop() → cancel + abort (RAII)

McpCatalog — 只是 Arc<CatalogStore>，无后台任务
CatalogRefresher — 唯一写接口，Watcher 通过 trait 调用
```

**关键决策（Grill 2026-07-12）：**

- **`ManagedServer` 实现 `Drop`（RAII）** — `cancel.cancel()` + `watcher.abort()` 统一在 `ManagedServer::drop()` 中完成。
- **`remove()` 只需 `shift_remove()`** — 取出的 `ManagedServer` 自动触发 `drop()`，无需手工 cancel。
- **`Registry::drop()` 只需 `servers.clear()`** — 依赖 `ManagedServer::drop()` 完成资源回收。
- **`shutdown()` 保留为异步优雅关闭** — 先尝试 `cancel()` → 等待 → `abort()`，用于需要优雅关闭的场景。

### ToolCatalog 抽象

```rust
pub trait ToolCatalog: Send + Sync {
    async fn snapshot(&self) -> Arc<ToolSnapshot>;
}
```

**McpCatalog 内部使用 `Arc<CatalogStore>`** — Watcher refresh 时 store，外部 `snapshot()` 直接 load，零拷贝。

### 刷新机制（双模）

- **Push** — `notifications/tools/list_changed` → **McpCatalogWatcher** 收到 → 通过 `CatalogRefresh` trait 调用 `CatalogRefresher.refresh()` → `CatalogStore::store(new_snapshot)`
- **Pull** — 用户显式调用 `refresher.refresh()`

### CatalogRefresh trait — Command Pattern

```rust
#[async_trait]
pub trait CatalogRefresh: Send + Sync {
    async fn refresh(&self) -> Result<(), McpError>;
}
```

**为什么用 trait：** Watcher 不依赖具体类型，只依赖刷新能力。这使得：
- Watcher 可以复用不同的刷新实现（测试时可用 Mock）
- 职责更清晰：Watcher 只负责监听通知，刷新逻辑由 CatalogRefresher 实现
- 避免 Watcher 持有 Client 或 Catalog 的 Arc

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
| Request ID 唯一来源是 McpClient | Transport 永远不生成/修改 ID，只传输已带 id 的 Request |
| Transport 读取循环退出时必须发送 Disconnected | 确保 fail-fast 检查有效，不再向死亡连接写入数据 |
| ManagedServer 实现 Drop (RAII) | CancellationToken drop 不会自动 cancel，必须显式 cancel + abort |
| Registry 不实现 ToolCatalog | Registry 管生命周期，RegistryCatalog 管工具聚合 |
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
