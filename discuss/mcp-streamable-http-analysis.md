# MCP Streamable HTTP 对齐分析

> 日期：2026-07-30 | 状态：Draft
>
> **目标**：将 lellm-mcp 的 Transport 层对齐到 MCP Spec 2026-07-28（Streamable HTTP）

---

## 1. 现状总览

### 已有实现

| 组件 | 当前状态 | 对应规范 |
|------|---------|---------|
| `StdioTransport` | 完整，无需改动 | stdio transport |
| `HttpTransport` | **部分实现** Streamable HTTP | streamable-http（2025-03-26） |
| `SseTransport` | 完整实现旧规范 | HTTP+SSE（2024-11-05，**Deprecated**） |
| `SimpleMcp::run_http` | 简单 POST handler | 无（非标准） |
| `SimpleMcp::run_sse` | 旧 SSE 双端点 | HTTP+SSE（**Deprecated**） |
| 协议版本 | 硬编码 `"2024-11-05"` | 应支持 2026-07-28 |

### HttpTransport 已实现的功能

- [x] 单端点 POST
- [x] `Accept: application/json, text/event-stream`
- [x] SSE 响应解析（内联 parser）
- [x] `session-id` 自动捕获与携带
- [x] 通知 broadcast（从 SSE 响应中解析）

### HttpTransport 缺失的功能

- [ ] `MCP-Protocol-Version` header
- [ ] `Mcp-Method` / `Mcp-Name` 标准请求头
- [ ] `_meta` 注入（protocolVersion, clientInfo, clientCapabilities）
- [ ] 无状态模式（移除 session-id 依赖）
- [ ] `subscriptions/listen` 长连接支持
- [ ] MRTR（Multi Round-Trip Requests）支持
- [ ] 通知 POST 的 202 Accepted 处理
- [ ] `X-Accel-Buffering: no` header

---

## 2. MCP Spec 2026-07-28 核心变更

### 2.1 删除协议级 Sessions

**之前（2025-03-26 ~ 2025-11-25）**：
- 服务器返回 `Mcp-Session-Id` header
- 客户端后续请求携带此 header

**现在（2026-07-28）**：
- 删除 `Mcp-Session-Id`
- 完全无状态
- 需要跨调用状态的 Server 使用显式 handle（作为 tool 参数传递）

**影响**：`HttpTransport` 中的 `session_id: Mutex<Option<String>>` 不再需要。

### 2.2 删除 Initialize 握手

**之前**：
```
initialize → notifications/initialized → 正常通信
```

**现在**：
- 每请求自包含 `_meta`
- `server/discover` 替代 initialize（可选，用于版本发现）
- 协议版本通过 `MCP-Protocol-Version` header + `_meta` 双重携带

**影响**：
- `McpClient::initialize()` 需要改为可选的兼容模式
- `JsonRpcRequest` 需要支持 `_meta` 注入
- 新增 `server/discover` 方法

### 2.3 请求元数据 Headers

每个 POST 必须携带：

| Header | 来源 | 必须 |
|--------|------|------|
| `MCP-Protocol-Version` | 协议版本 | 是 |
| `Mcp-Method` | `method` 字段 | 是 |
| `Mcp-Name` | `params.name` 或 `params.uri` | tools/call, resources/read, prompts/get |

**影响**：`HttpTransport::request()` 需要从 `JsonRpcRequest` 中提取并设置这些 headers。

### 2.4 MRTR（Multi Round-Trip Requests）

**之前**：服务器在 SSE 流上发送独立的 JSON-RPC request（如 sampling、elicitation）
**现在**：服务器返回 `InputRequiredResult`（`resultType: "input_required"`），客户端重试原请求并附带 `inputResponses`

**影响**：
- `JsonRpcResponse` / `JsonRpcResult` 需要支持 `resultType`
- 新增 `InputRequiredResult` 类型
- SSE 流上不再处理 server-initiated requests

### 2.5 Subscriptions

**之前**：`resources/subscribe` / `resources/unsubscribe`
**现在**：`subscriptions/listen` — 单一 POST 返回 SSE 长连接

**影响**：
- 新增 `subscriptions/listen` 方法
- HttpTransport 需要支持长寿命 SSE 订阅流

### 2.6 删除 GET 端点

**之前（2025-03-26）**：单端点支持 POST + GET
**现在（2026-07-28）**：只支持 POST

**影响**：无（当前实现未使用 GET）。

### 2.7 ResultType

所有结果必须携带 `resultType` 字段：
- `"complete"` — 普通结果
- `"input_required"` — MRTR 中间结果

---

## 3. 优化方案

### Phase 1：Client Transport 层（P0 — 立即）

#### 3.1 HttpTransport 头信息注入

```rust
// 修改 HttpTransport::request()
async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
    let mut builder = inner
        .client
        .post(&self.config.endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2026-07-28")  // 新增
        .header("Mcp-Method", &req.method_name)         // 新增
        .timeout(self.config.request_timeout);

    // 新增：Mcp-Name header
    if let Some(name) = extract_mcp_name(&req.method_name, &req.params) {
        builder = builder.header("Mcp-Name", &name);
    }

    // 删除：session-id 相关逻辑
    // if let Some(ref sid) = *inner.session_id.lock().await { ... }
}
```

#### 3.2 移除 Session-ID

```rust
struct HttpTransportInner {
    client: reqwest::Client,
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
    // session_id: Mutex<Option<String>>,  // 删除
}
```

#### 3.3 更新 Config

```rust
pub struct HttpConfig {
    pub endpoint_url: String,
    pub request_timeout: std::time::Duration,
    pub protocol_version: String,  // 新增，默认 "2026-07-28"
}
```

### Phase 2：协议层（P1 — 紧随）

#### 2.1 更新协议版本常量

```rust
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const PING: &str = "ping";
    pub const SERVER_DISCOVER: &str = "server/discover";  // 新增
    pub const SUBSCRIPTIONS_LISTEN: &str = "subscriptions/listen";  // 新增
}
```

#### 2.2 支持 `_meta` 注入

```rust
pub struct RequestMeta {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    #[serde(rename = "clientInfo", skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ImplementationInfo>,
    #[serde(rename = "clientCapabilities", skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<serde_json::Value>,
}
```

`JsonRpcRequest` 的 params 中自动合并 `_meta`。

#### 2.3 支持 MRTR

```rust
pub struct JsonRpcResult {
    pub result_type: String,  // "complete" | "input_required"
    // ... existing fields
}

pub struct InputRequiredResult {
    pub result_type: String,  // "input_required"
    pub input_requests: Vec<InputRequest>,
}
```

### Phase 3：Server 层（P2 — 后续）

#### 3.1 SimpleMcp::run_http 改为 Streamable HTTP

```rust
pub async fn run_streamable_http(
    self,
    port: u16,
) -> Result<(), super::ServerError> {
    // 单端点 POST /mcp
    // 支持 JSON 和 SSE 两种响应
    // 支持 subscriptions/listen
    // 不维护 session 状态
}
```

#### 3.2 删除 run_sse

标记 `run_sse()` 为 deprecated，引导用户使用 `run_streamable_http()`。

#### 3.3 删除 initialize 握手

Server 不再要求 initialize，每个请求自包含版本信息。

### Phase 4：兼容层（P3 — 长期）

#### 4.1 向后兼容

对于连接旧版 Server 的场景：
- 检测 Server 是否支持 `MCP-Protocol-Version`
- 自动降级到 2024-11-05 模式（session-id + initialize）
- 通过 `server/discover` 进行版本探测

#### 4.2 SseTransport 废弃路径

```rust
#[deprecated(
    since = "0.5.0",
    note = "HTTP+SSE transport is deprecated per MCP 2026-07-28. Use HttpTransport (Streamable HTTP) instead."
)]
pub struct SseTransport { ... }
```

---

## 4. SseTransport 处置

### 现状
- 完整实现了 HTTP+SSE（2024-11-05）
- 双端点模式（GET SSE + POST messages）
- 依赖 `endpoint` 事件获取 POST URL

### 建议
1. **标记 deprecated** — 加 `#[deprecated]` attribute
2. **保留编译** — 给已有用户迁移时间
3. **文档明确** — Cargo doc 和 README 引导到 HttpTransport
4. **feature gate** — `sse` feature 在下次大版本中移除

---

## 5. 文件变更清单

| 文件 | 变更 | 优先级 |
|------|------|--------|
| `transport/http.rs` | 加 headers、删 session-id、加 config | P0 |
| `transport/mod.rs` | TransportCapabilities 扩展 | P0 |
| `protocol/request.rs` | 加 `_meta`、加新方法常量 | P1 |
| `protocol/response.rs` | 加 `resultType`、`InputRequiredResult` | P1 |
| `protocol/notification.rs` | 加 subscriptions 相关通知 | P1 |
| `client/mod.rs` | `_meta` 注入、discover、subscriptions/listen | P1 |
| `server/handler.rs` | Streamable HTTP handler、删 SSE | P2 |
| `server/simple.rs` | `run_streamable_http`、deprecate `run_sse` | P2 |
| `Cargo.toml` | deprecate `sse` feature | P3 |

---

## 6. 风险与注意事项

### 6.1 向后兼容

- 大量 MCP Server 仍使用 2024-11-05 或 2025-03-26 版本
- 需要版本协商机制，不能一刀切
- 建议：`HttpConfig` 支持 `protocol_version` 配置，默认最新，可降级

### 6.2 Server 发现

- `server/discover` 是可选的
- 对于已知 Server，直接指定版本
- 对于未知 Server，先 discover 再决定

### 6.3 测试覆盖

- 需要 mock Server 测试不同协议版本
- SSE 响应解析已有内联 parser，需确保兼容性
- MRTR 需要端到端测试

---

## 7. 时间线建议

| 阶段 | 内容 | 预估工作量 |
|------|------|-----------|
| **Phase 1** | HttpTransport headers + 删 session-id | 1-2 小时 |
| **Phase 2** | 协议层 `_meta` + MRTR | 2-3 小时 |
| **Phase 3** | Server Streamable HTTP | 2-3 小时 |
| **Phase 4** | 兼容层 + 版本协商 | 3-4 小时 |
| **总计** | | 8-12 小时 |
