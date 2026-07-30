# MCP Transport 使用指南

> 版本：v0.3 | 日期：2026-07-30 | 基于 MCP Spec 2026-07-28

## 概述

LeLLM 的 MCP 模块支持两种传输方式：

1. **StdioTransport** — 通过本地子进程 stdin/stdout 通信
2. **HttpTransport** — Streamable HTTP（**推荐**，MCP Spec 2026-07-28）

> **注意**：SseTransport（HTTP+SSE）已在 MCP 2026-07-28 中被标记为 Deprecated，
> 相关 API 已加 `#[deprecated]`。仅连接旧版 MCP Server 时才使用。

## 传输方式对比

| 特性 | StdioTransport | HttpTransport |
|------|----------------|---------------|
| **规范状态** | Active | **Active（推荐）** |
| **连接方式** | 本地子进程 | 单端点 HTTP POST |
| **服务器状态** | 有状态 | **无状态** |
| **负载均衡** | N/A | **天然支持** |
| **请求取消** | 不明确 | 关闭 SSE 响应流 = 取消 |
| **通知推送** | broadcast | subscriptions/listen |
| **延迟** | 中 | 低 |
| **稳定性** | 高 | 高 |
| **适用场景** | 本地 MCP Server | **远程 MCP Server（首选）** |

## 推荐：HttpTransport（Streamable HTTP）

Streamable HTTP 于 MCP 2025-03-26 引入，取代 HTTP+SSE。2026-07-28 版本进一步强化了无状态设计。

### 核心特性

- **单端点** — 只需一个 `POST /mcp` 端点
- **无状态** — 无需维护会话映射，天然支持水平扩展
- **标准 Headers** — `MCP-Protocol-Version`、`Mcp-Method`、`Mcp-Name` 让 LB/Gateway 可直接路由
- **Accept 感知** — 自动检测 Accept header，返回 JSON 或 SSE 响应
- **server/discover** — 版本发现，支持版本协商（返回支持的版本交集）
- **subscriptions/listen** — 统一订阅机制，SSE 长连接 + acknowledgement 通知

### 使用示例

```rust
use lellm_mcp::transport::{HttpConfig, HttpTransport};

let config = HttpConfig::new("https://mcp.map.qq.com/mcp?key=your_api_key&format=0")
    .with_request_timeout(std::time::Duration::from_secs(60));

let transport = HttpTransport::new(config);
```

### 便捷构造

```rust
use lellm_mcp::McpClient;

// 一行代码连接
let client = McpClient::connect_http("https://mcp.map.qq.com/mcp?key=xxx").await?;
```

## StdioTransport（本地）

```rust
use lellm_mcp::transport::{StdioConfig, StdioTransport};

let config = StdioConfig::new("npx", vec![
    "-y".to_string(),
    "@baidumap/mcp-server-baidu-map".to_string(),
])
.with_env(vec![
    ("BAIDU_MAP_API_KEY".to_string(), "your_api_key".to_string()),
]);

let transport = StdioTransport::new(config);
```

## Server 端使用

### SimpleMcp 服务器

```rust
use lellm_mcp::server::SimpleMcp;

let mut mcp = SimpleMcp::new("My Server");

mcp.tool("add", "Add two numbers", |args: serde_json::Value| async move {
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    Ok(serde_json::json!({ "result": a + b }))
});

// Streamable HTTP（推荐）
mcp.run_http(3100).await?;

// Stdio
mcp.run_stdio().await?;
```

### 协议版本支持

Server 端 `server/discover` 返回支持的版本列表：
`["2026-07-28", "2025-11-25", "2025-03-26", "2024-11-05"]`

客户端指定版本列表时，返回交集。

## QQ 地图 MCP 配置

### 获取 API Key

1. 访问 https://lbs.qq.com/service/webService/webServiceGuide/overview
2. 注册并创建 API Key
3. 开启 WebServiceAPI 功能

### 传输端点

- **Streamable HTTP（推荐）**: `https://mcp.map.qq.com/mcp?key=YOUR_KEY&format=0`

### 参数说明

| 参数 | 必填 | 说明 |
|------|------|------|
| key | 是 | 开发者 API Key |
| format | 否 | 返回格式：0=语义化文本（默认），1=原始 JSON |

## Feature Gates

在 `Cargo.toml` 中启用需要的 feature：

```toml
[dependencies]
lellm-mcp = { version = "0.4", features = ["http"] }     # Streamable HTTP（推荐）
```

## 错误处理

| 错误类型 | 描述 | 处理方式 |
|---------|------|---------|
| `TransportError::Http` | HTTP 请求失败 | 检查网络和 URL |
| `TransportError::Timeout` | 请求超时 | 增加超时时间 |
| `TransportError::Disconnected` | 连接断开 | 重新连接 |
| `McpError::Protocol` | 协议错误 | 检查请求格式 |

## MCP 规范版本说明

| 规范版本 | 传输方式 | 状态 |
|---------|---------|------|
| 2024-11-05 | HTTP+SSE（双端点） | **Deprecated** |
| 2025-03-26 | Streamable HTTP（单端点） | Active |
| 2026-07-28 | Streamable HTTP（无状态，无 sessions） | **最新，Active** |

lellm-mcp 的 HttpTransport 和 Server 端已完全对齐 MCP 2026-07-28 规范。
