# ADR-003: MCP Transport 抽象

> **日期**: 2026-06-12
> **状态**: Proposed — 待评审
> **依赖**: [ADR-001](./001-mcp-as-tool-runtime-extension.md), [ADR-002](./002-mcp-crate-structure.md)

## 背景

MCP 协议基于 JSON-RPC 2.0，需要通过网络传输 request/response/notification。
不同传输介质（stdio、SSE、HTTP）有不同的 IO 模型。
需要抽象 Transport trait，让 McpClient 不关心底层传输。

**核心设计原则**：
- MCP 90% 是 request-response，notification 是补充
- request-id 匹配由 Transport 内部完成，不对上暴漏
- 重连由 McpClient 管理，Transport 只负责单次连接的生命周期

## 决策

### Transport Trait

```rust
use async_trait::async_trait;
use futures::Stream;

#[async_trait]
pub trait McpTransport: Send + Sync + 'static {
    /// Notification 输出流（progress, tools/list_changed 等）
    type Stream: Stream<Item = JsonRpcNotification> + Send + 'static;

    /// 建立连接
    /// - stdio: 启动子进程，初始化读写 loop
    /// - SSE: 建立 HTTP 连接，开始接收 events
    async fn connect(&self) -> Result<(), McpError>;

    /// 发送 JSON-RPC Request，等待对应 Response
    /// - 内部处理 request-id 生成与匹配
    /// - 超时由 timeout 参数控制
    /// - 连接断开返回 McpError::Disconnected
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<JsonRpcResponse, McpError>;

    /// 获取 notification 流
    /// - 调用者可选消费（不消费则静默丢弃）
    /// - 内部使用有界 channel，满时丢弃最新（背压）
    fn notifications(&self) -> Self::Stream;

    /// 主动断开连接
    /// - stdio: 终止子进程
    /// - SSE: 关闭 HTTP 连接
    async fn close(&self) -> Result<(), McpError>;
}
```

### 设计理由

**为什么不是 `send()` + `receive()` 双接口？**

双接口要求调用者自己管理 request-id 匹配：
```rust
// ❌ 不要这样 — 暴露过多细节
let id = next_id();
transport.send(Request { id, method, params }).await?;
let resp = transport.receive().await?;
// 调用者需要循环直到 id 匹配
```

单 `request()` 接口封装了请求-响应语义：
```rust
// ✅ 这样 — 语义清晰
let resp = transport.request("tools/list", json!({}), None).await?;
```

**为什么 notification 是 Stream 而不是回调？**

- Stream 是推拉结合：消费者控制消费速度，生产者通过 channel 背压
- 回调模型难以组合（多个消费者？）
- MCP notification 频率低（progress, list_changed），Stream 开销可忽略

---

## 关键接口深挖

### 1. Request-ID 管理

**决策**：Transport 内部管理 monotonic counter。

```rust
struct TransportInner {
    next_id: AtomicU64,       // monotonic increment
    pending: Mutex<PendingMap>, // id -> oneshot channel
}

impl TransportInner {
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>)
        -> Result<JsonRpcResponse, McpError>
    {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();

        // 注册 pending
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        // 发送请求
        let msg = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: "2.0",
            id: id,
            method: method.to_string(),
            params,
        });
        self.send_raw(&msg).await?;

        // 等待响应
        let result = match timeout {
            Some(d) => tokio::time::timeout(d, rx).await
                .map_err(|_| McpError::Timeout)?,
            None => rx.await.map_err(|_| McpError::Disconnected)?,
        };

        // 清理 pending
        {
            let mut pending = self.pending.lock().await;
            pending.remove(&id);
        }

        result
    }
}
```

**read_loop 如何分发？**

```rust
async fn read_loop(&self) {
    while let Ok(msg) = self.read_one().await {
        match msg {
            JsonRpcMessage::Response(resp) => {
                // 路由到对应的 pending rx
                if let Some(tx) = self.pending.lock().await.remove(&resp.id) {
                    let _ = tx.send(resp.result);
                }
                // 找不到 id → 日志警告（应该是 bug）
            }
            JsonRpcMessage::Notification(notif) => {
                // 推入 notification channel（满则丢弃）
                let _ = self.notification_tx.send(notif).await;
            }
            JsonRpcMessage::Request(_) => {
                // Client 端不应该收到 Request（那是 Server 的事）
                // 记录警告，忽略
            }
        }
    }
    // 读循环结束 = 连接断开
    // 清除所有 pending，让它们收到 Disconnected 错误
    self.pending.lock().await.clear();
}
```

### 2. 重连

**决策**：重连由 `McpClient` 管理，不在 Transport 层。

Transport 只负责单次连接的生命周期：`connect()` → 使用 → `close()`。

```rust
impl McpClient {
    async fn call_with_reconnect(
        &self,
        method: &str,
        params: Value,
        max_retries: u32,
    ) -> Result<JsonRpcResponse, McpError> {
        let mut last_error = McpError::Disconnected;

        for attempt in 0..=max_retries {
            match self.transport.request(method, params.clone(), self.timeout).await {
                Ok(resp) => return Ok(resp),
                Err(McpError::Disconnected | McpError::Timeout) => {
                    if attempt >= max_retries {
                        break;
                    }

                    // 指数退避
                    let delay = Duration::from_millis(
                        (2u64.pow(attempt)) .min(MAX_RETRY_DELAY_MS)
                    );
                    tokio::time::sleep(delay).await;

                    // 重新连接
                    self.transport.close().await.ok();
                    match self.transport.connect().await {
                        Ok(()) => {
                            // 重新初始化
                            self.initialize().await?;
                        }
                        Err(e) => {
                            last_error = e;
                            continue;
                        }
                    }
                }
                Err(e) => return Err(e),  // 非重试错误，直接返回
            }
        }

        Err(last_error)
    }
}
```

**重试策略**：
- 可重试错误：`Disconnected`, `Timeout`
- 不可重试错误：`Protocol`, `InvalidParams`（重试无意义）
- 指数退避：100ms → 200ms → 400ms → ... → max 5s
- 最大重试次数：5 次（可配置）

**为什么不在 Transport 层？**
- 重连后需要重新 `initialize`（这是 MCP 协议层逻辑，不是传输层）
- 不同调用者可能有不同的重试偏好（discover 可以重试，tool call 可能不想）
- Transport 应保持单次连接的纯粹性

### 连接状态机

**决策**：状态机由 `McpClient` 管理，Transport 不感知。

```rust
pub enum ConnState {
    Disconnected,   // 初始状态 / close() 后
    Connecting,     // connect() 中
    Initializing,   // 发送 initialize 请求
    Ready,          // 可用
    Broken,         // 检测到异常，等待重连
    Closed,         // 不可恢复的关闭
}

impl ConnState {
    pub fn allows_request(&self) -> bool {
        matches!(self, ConnState::Ready)
    }
}
```

**request() Fail-fast**：非 Ready 状态直接返回 `McpError::Disconnected`，不阻塞。
- Agent 已有 `RetryPolicy`，调用层可以决定重试
- 阻塞会违反 `ToolTimeout` 契约
- 语义清晰：`Disconnected` 错误让调用者自己做决策

**request-id 连续性**：`next_id` 是 monotonic counter（`AtomicU64`），重连不重置。
64-bit counter，每秒 1000 请求，运行 300 年才溢出。

**notification 不重放**：重连后是全新连接，MCP 协议不支持 notification replay。
错过 `tools/list_changed` → 下次显式 `refresh_tools()` 会 pull 最新列表。

### 3. Heartbeat (Ping/Pong)

**决策**：McpClient 层定时 ping，Transport 不感知。

MCP 协议定义了 `ping` method：
- Client 发 `ping`（无 params），Server 必须返回空 response
- Server 发 `ping`，Client 必须回应（由 read_loop 处理）

```rust
impl McpClient {
    /// 启动心跳 loop
    async fn heartbeat_loop(&self) {
        let interval = self.config.heartbeat_interval;

        let mut interval = tokio::time::interval(interval);
        loop {
            interval.tick().await;

            match self.transport.request("ping", json!({}), None).await {
                Ok(_) => { /* alive */ }
                Err(McpError::Disconnected) => {
                    // 标记为 broken，触发重连
                    self.state.set(ConnState::Broken);
                    break;
                }
                Err(_) => { /* 非致命，忽略 */ }
            }
        }
    }
}
```

**心跳配置**：
- 默认间隔：30s
- 可配置（`McpClientConfig::heartbeat_interval`）
- 心跳失败不立即重连，等实际请求失败时再处理

**为什么不在 Transport 层？**
- `ping` 是 MCP 协议方法，不是传输层概念
- stdio 管道存活 ≠ Server 活着（Server 可能卡死但进程还在）
- Transport 不应该知道 MCP 协议细节

### 4. Stdio 子进程管理

**决策**：`StdioTransport` 封装子进程生命周期。

```rust
pub struct StdioTransport {
    config: StdioConfig,
    inner: Option<StdioTransportInner>,
}

pub struct StdioTransportInner {
    #[allow(dead_code)]
    child: Child,                    // 持有子进程句柄
    pending: Mutex<HashMap<u64, oneshot::Channel<JsonRpcResponse>>>,
    notification_tx: mpsc::Sender<JsonRpcNotification>,
    notification_rx: mpsc::Receiver<JsonRpcNotification>,
    next_id: AtomicU64,
    shutdown: watch::Sender<bool>,   // 通知 read_loop 退出
}

pub struct StdioConfig {
    pub command: String,             // 命令（如 "npx"）
    pub args: Vec<String>,           // 参数（如 ["@modelcontextprotocol/server-filesystem", "/path"]）
    pub env: Option<HashMap<String, String>>,
    pub startup_timeout: Duration,   // 子进程启动超时
}
```

**connect() 实现**：

```rust
#[async_trait]
impl McpTransport for StdioTransport {
    async fn connect(&self) -> Result<(), McpError> {
        let mut child = tokio::process::Command::new(&self.config.command)
            .args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())     // stderr 丢弃（或可选捕获）
            .spawn()
            .map_err(|e| McpError::Io(e))?;

        // 启动超时保护
        let child_handle = child.id();
        if let Some(timeout) = self.config.startup_timeout {
            // 等待子进程就绪（通常通过第一次 write 确认）
            tokio::time::timeout(timeout, async {
                // stdin/stdout 已 piped，可以开始通信
            }).await
            .map_err(|_| McpError::Timeout)?;
        }

        let (notification_tx, notification_rx) = mpsc::channel(BUFFER_SIZE);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let inner = StdioTransportInner {
            child,
            pending: Mutex::new(HashMap::new()),
            notification_tx,
            notification_rx,
            next_id: AtomicU64::new(1),
            shutdown: shutdown_tx,
        };

        // 启动 read_loop
        inner.spawn_read_loop();

        self.inner = Some(inner);
        Ok(())
    }

    async fn request(&self, method: &str, params: Value, timeout: Option<Duration>)
        -> Result<JsonRpcResponse, McpError>
    {
        let inner = self.inner.as_ref().ok_or(McpError::Disconnected)?;
        inner.request(method, params, timeout).await
    }

    fn notifications(&self) -> Self::Stream {
        // 返回 notification_rx 的 stream adapter
        // 如果 inner 不存在，返回空 stream
        ...
    }

    async fn close(&self) -> Result<(), McpError> {
        if let Some(inner) = &self.inner {
            // 通知 read_loop 退出
            let _ = inner.shutdown.send(true);

            // 终止子进程
            inner.child.kill().await.ok();

            // 等待子进程退出
            inner.child.wait().await.ok();
        }
        Ok(())
    }
}
```

**read_loop 实现**：

```rust
impl StdioTransportInner {
    fn spawn_read_loop(&self) {
        let mut stdout = self.child.stdout.take().expect("stdout should be piped");
        let (notification_tx, shutdown) = (
            self.notification_tx.clone(),
            self.shutdown.clone(),
        );
        let pending = self.pending.clone();

        tokio::spawn(async move {
            let mut lines = stdout.lines();

            loop {
                tokio::select! {
                    // 关闭信号
                    _ = shutdown.changed() => {
                        break;
                    }

                    // 读取一行 JSON
                    result = lines.next_line() => {
                        let line = match result {
                            Ok(Some(line)) => line,
                            Ok(None) => break,  // EOF = 子进程退出
                            Err(e) => {
                                log::warn!("stdio read error: {}", e);
                                break;
                            }
                        };

                        // 解析 JSON-RPC message
                        let msg: JsonRpcMessage = match serde_json::from_str(&line) {
                            Ok(msg) => msg,
                            Err(e) => {
                                log::warn!("invalid jsonrpc message: {}", e);
                                continue;
                            }
                        };

                        // 分发（见上文 request-id 管理）
                        dispatch_message(&pending, &notification_tx, msg);
                    }
                }
            }

            // read_loop 退出 → 清除所有 pending
            pending.lock().await.clear();
        });
    }
}
```

**子进程启动命令示例**：

```rust
// npx @modelcontextprotocol/server-filesystem /path/to/dir
StdioConfig {
    command: "npx".to_string(),
    args: vec![
        "@modelcontextprotocol/server-filesystem".to_string(),
        "/path/to/dir".to_string(),
    ],
    env: None,
    startup_timeout: Duration::from_secs(10),
}
```

### 5. Notification 背压

**决策**：有界 channel + 满时丢弃最新。

```rust
// channel 容量
const NOTIFICATION_BUFFER: usize = 64;

// read_loop 中推送 notification
async fn push_notification(
    tx: &mpsc::Sender<JsonRpcNotification>,
    notif: JsonRpcNotification,
) {
    match tx.send(notif).await {
        Ok(()) => {},
        Err(mpsc::error::SendError(_)) => {
            // 接收端已关闭，read_loop 应该退出
        }
    }
}

// 如果需要主动丢弃（channel 满时）：
async fn push_or_drop(
    tx: &mpsc::Sender<JsonRpcNotification>,
    notif: JsonRpcNotification,
) {
    match tx.reserve().await {
        Some(permit) => { let _ = permit.send(notif); }
        None => { /* 接收端关闭 */ }
    }
}
```

**理由**：
- MCP notification 频率低（progress notification 是唯一高频的，且 tool call 期间才产生）
- 64 容量足够缓冲 burst
- 丢弃 notification 比阻塞 request-response 更安全
- 消费者不关心 notification → 静默丢弃（零开销）

**Notification 类型**（v0.3）：
- `notifications/progress` — 工具调用进度（未来可能用于 streaming tool）
- `notifications/tools/list_changed` — 工具列表变更提示
- 其他 — 忽略

### 6. Tool Timeout

**决策**：Timeout 贯穿三层，各司其职。

```
McpClientConfig.timeout (默认 60s)
    ↓ 传入
Transport.request(timeout)
    ↓ tokio::time::timeout
ToolRegistration func (不受限，由 Agent RetryPolicy 控制)
```

**各层 Timeout**：

| 层级 | 默认值 | 作用 | 覆盖方式 |
|------|--------|------|----------|
| `McpClientConfig.timeout` | 60s | 单次 MCP 请求超时 | `client.with_timeout()` |
| `Transport.request(timeout)` | 同 client | JSON-RPC 请求超时 | 方法级 override |
| `Agent RetryPolicy.deadline` | 无 | 工具调用总超时（含重试）| `RetryPolicy::with_deadline()` |

**initialize 特殊处理**：
```rust
// initialize 通常很快，给短 timeout
let init_resp = self.transport.request(
    "initialize",
    init_params,
    Some(Duration::from_secs(10)),  // 10s
).await?;
```

**tools/call 特殊处理**：
```rust
// tool call 可能很慢（网络请求、文件操作），用默认 timeout
let call_resp = self.transport.request(
    "tools/call",
    call_params,
    self.timeout,  // 60s default
).await?;
```

---

## McpError 定义

```rust
pub enum McpError {
    /// 连接已断开（子进程退出、网络断开）
    Disconnected,
    /// 请求超时
    Timeout,
    /// JSON-RPC 协议错误（格式不对、版本不匹配）
    Protocol(String),
    /// 参数无效（Server 返回 -32602）
    InvalidParams(String),
    /// Server 内部错误（Server 返回 -32603）
    ServerError(String),
    /// 方法未找到（Server 返回 -32601）
    MethodNotFound(String),
    /// IO 错误（子进程启动失败、管道断裂）
    Io(io::Error),
}

impl McpError {
    /// 是否值得重试
    pub fn is_retriable(&self) -> bool {
        match self {
            McpError::Disconnected | McpError::Timeout => true,
            McpError::Protocol(_)
            | McpError::InvalidParams(_)
            | McpError::MethodNotFound(_)
            | McpError::ServerError(_)
            | McpError::Io(_) => false,
        }
    }
}
```

---

## 连接状态机

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Initializing,   // 发送 initialize 请求
    Ready,          // 可用
    Broken,         // 连接异常，需要重连
}

impl ConnState {
    pub fn allows_request(&self) -> bool {
        matches!(self, ConnState::Ready)
    }
}
```

**状态转换**：
```
Disconnected → connect() → Connecting → Initialized → Ready
                                            ↓ (失败)
                                         Broken

Ready → (request 失败) → Broken
Broken → (重连) → Connecting → ...
Ready → (close()) → Disconnected
```

**状态由 McpClient 管理**，Transport 不感知状态机。

---

## 后果

### 正面
- Transport trait 面积极小（4 方法），易于实现新传输
- request-id 内部化，调用者无需管理
- notification 流式消费，不阻塞主流程
- 重连在 Client 层，可以配合 MCP 协议（重新 initialize）
- 各层 timeout 独立配置，灵活控制

### 负面
- `StdioTransport` 需要内部 read_loop（spawn task），增加复杂度
- notification 丢弃策略可能丢失进度信息（但 v0.3 不用 progress）
- 重连逻辑在 Client 层，每个调用点需要处理（可通过封装缓解）

### 风险
- stdio 子进程在不同平台的行为差异（Windows 上 `npx` 可用性）
- JSON-RPC 消息可能跨行（理论上 MCP 用 newline-delimited JSON，但需验证）
- 子进程 stderr 丢弃可能丢失调试信息

## 未来演进

- v0.4: SSE Transport 实现
- v0.4: HTTP Transport 实现
- v0.5: Sampling 需要 Server→Client Request（Transport 需要支持双向请求）
