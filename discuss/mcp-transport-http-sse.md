# lellm-mcp: SSE/HttpTransport 实现规划

> 版本：v0.1 | 日期：2026-07-03 | 状态：规划中

## 一、背景

### 当前限制
LeLLM 的 MCP 实现只有 `StdioTransport`，通过启动本地子进程通信。这导致：
- 需要本地安装 Node.js/Python
- 启动进程有额外开销
- 无法直连远程 MCP 服务器

### 目标
实现 SSE 和 HttpTransport，支持直连远程 MCP 服务器（如百度地图 MCP）。

### 百度地图 MCP 支持的传输方式
1. **Streamable HTTP（推荐）**：`https://mcp.map.baidu.com/mcp?ak=YOUR_AK`
2. **SSE**：`https://mcp.map.baidu.com/sse?ak=YOUR_AK`
3. **stdio（本地）**：`npx -y @baidumap/mcp-server-baidu-map`

## 二、技术方案

### 2.1 架构设计

```
lellm-mcp/src/transport/
├── mod.rs           # McpTransport trait
├── state.rs         # ConnectionState
├── stdio.rs         # StdioTransport（已有）
├── sse.rs           # SseTransport（新增）
└── http.rs          # HttpTransport（新增）
```

### 2.2 McpTransport Trait 回顾

```rust
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn connect(&mut self) -> Result<(), McpError>;
    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError>;
    fn notifications(&self) -> NotificationStream;
    async fn close(&mut self) -> Result<(), McpError>;
    fn state(&self) -> watch::Receiver<ConnectionState>;
}
```

### 2.3 SSE Transport 设计

**原理**：
- 使用 SSE (Server-Sent Events) 接收服务器推送
- 使用 HTTP POST 发送请求
- 单向通道：服务器 → 客户端（SSE），客户端 → 服务器（HTTP POST）

**实现要点**：
```rust
pub struct SseTransport {
    /// SSE 端点 URL（如 https://mcp.map.baidu.com/sse?ak=xxx）
    sse_url: String,
    /// 请求端点 URL（从 SSE 事件中获取）
    post_url: Option<String>,
    /// 连接状态
    state: watch::Sender<ConnectionState>,
    /// 待处理的请求
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<JsonRpcResponse, McpError>>>>>,
    /// Notification 发送器
    notification_tx: broadcast::Sender<JsonRpcNotification>,
    /// HTTP 客户端
    client: reqwest::Client,
    /// SSE 连接句柄
    sse_handle: Option<JoinHandle<()>>,
}
```

**SSE 事件处理**：
```
事件类型:
- endpoint: 获取 POST URL
- message: JSON-RPC Response 或 Notification
```

### 2.4 HttpTransport (Streamable HTTP) 设计

**原理**：
- 单一 HTTP 端点
- 请求和响应都通过 HTTP POST
- 支持无状态通信

**实现要点**：
```rust
pub struct HttpTransport {
    /// HTTP 端点 URL（如 https://mcp.map.baidu.com/mcp?ak=xxx）
    endpoint_url: String,
    /// 连接状态
    state: watch::Sender<ConnectionState>,
    /// 待处理的请求
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<JsonRpcResponse, McpError>>>>>,
    /// Notification 发送器
    notification_tx: broadcast::Sender<JsonRpcNotification>,
    /// HTTP 客户端
    client: reqwest::Client,
}
```

### 2.5 Feature Gate

```toml
[features]
default = ["stdio", "bridge"]
stdio = []
sse = ["dep:reqwest"]
http = ["dep:reqwest"]
bridge = ["dep:lellm-agent"]
```

## 三、实现步骤

### Phase 1: 基础设施
1. 添加 `reqwest` 依赖
2. 创建 `SseTransport` 和 `HttpTransport` 模块
3. 实现基础连接逻辑

### Phase 2: SSE Transport
1. 实现 SSE 连接和事件解析
2. 实现 POST 请求发送
3. 实现请求-响应匹配
4. 实现 Notification 流

### Phase 3: HttpTransport
1. 实现 HTTP POST 请求
2. 实现响应解析
3. 实现请求-响应匹配

### Phase 4: 集成测试
1. 使用百度地图 MCP 服务器测试
2. 测试天气查询工具调用
3. 测试错误处理

### Phase 5: 示例和文档
1. 创建 `examples/mcp_weather_sse.rs`
2. 创建 `examples/mcp_weather_http.rs`
3. 更新文档

## 四、配置示例

### SSE Transport
```rust
let config = SseConfig::new("https://mcp.map.baidu.com/sse?ak=YOUR_AK");
let transport = SseTransport::new(config);
```

### HttpTransport
```rust
let config = HttpConfig::new("https://mcp.map.baidu.com/mcp?ak=YOUR_AK");
let transport = HttpTransport::new(config);
```

## 五、错误处理

| 错误场景 | 处理方式 |
|---------|---------|
| 网络连接失败 | 返回 `McpError::Network` |
| 请求超时 | 返回 `McpError::Timeout` |
| SSE 连接断开 | 自动重连或返回 `McpError::Disconnected` |
| JSON 解析错误 | 返回 `McpError::Protocol` |

## 六、性能考虑

1. **连接池**：使用 `reqwest::Client` 的连接池
2. **超时设置**：可配置请求超时
3. **重连机制**：SSE 断开时自动重连
4. **背压处理**：Notification channel 使用有界 buffer

## 七、兼容性

1. 保持与现有 `StdioTransport` 的 API 一致性
2. 实现相同的 `McpTransport` trait
3. 支持相同的 Feature Gate 机制

## 八、风险和缓解

| 风险 | 缓解措施 |
|------|---------|
| SSE 连接不稳定 | 实现自动重连机制 |
| 百度地图 API 变更 | 抽象 HTTP 层，易于适配 |
| 性能问题 | 使用连接池、异步处理 |

## 九、下一步

1. 确认技术方案
2. 实现 SSE Transport
3. 实现 HttpTransport
4. 创建示例代码
5. 集成测试
