# MCP Transport 使用指南

> 版本：v0.2 | 日期：2026-07-30 | 基于 MCP Spec 2026-07-28

## 概述

LeLLM 的 MCP 模块支持三种传输方式：

1. **StdioTransport** — 通过本地子进程 stdin/stdout 通信
2. **HttpTransport** — Streamable HTTP（**推荐**，MCP Spec 2025-03-26+）
3. **SseTransport** — HTTP+SSE（**已废弃**，MCP Spec 2024-11-05，仅兼容旧服务器）

## 传输方式对比

| 特性 | StdioTransport | HttpTransport | SseTransport（废弃） |
|------|----------------|---------------|---------------------|
| **规范状态** | Active | **Active（推荐）** | Deprecated |
| **连接方式** | 本地子进程 | 单端点 HTTP POST | 双端点 SSE + POST |
| **服务器状态** | 有状态 | **无状态** | 有状态（会话映射） |
| **初始化握手** | 需要 initialize | 需要 initialize（兼容模式） | 需要 initialize |
| **负载均衡** | N/A | **天然支持** | 不支持（会话绑定） |
| **请求取消** | 不明确 | 关闭 SSE 响应流 = 取消 | 不明确 |
| **通知推送** | broadcast | subscriptions/listen | SSE 全局流 |
| **延迟** | 中 | 低 | 低 |
| **稳定性** | 高 | 高 | 中 |
| **适用场景** | 本地 MCP Server | **远程 MCP Server（首选）** | 兼容旧服务器 |

## 推荐：HttpTransport（Streamable HTTP）

Streamable HTTP 于 MCP 2025-03-26 引入，取代 HTTP+SSE。2026-07-28 版本进一步强化了无状态设计。

### 核心优势

- **单端点** — 只需一个 HTTP POST 端点
- **无状态** — 无需维护会话映射，天然支持水平扩展
- **中间件友好** — `Mcp-Method`/`Mcp-Name` 等 Header 让 LB/Gateway 可直接路由
- **请求级 SSE** — 服务器可选择返回单条 JSON 或 SSE 流，scoped 到当前请求

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

## SseTransport（已废弃，仅兼容旧服务器）

> **警告**：HTTP+SSE 已在 MCP 2026-07-28 中被标记为 Deprecated。
> 仅在你必须连接旧版 MCP Server 时使用。新 Server 应迁移到 Streamable HTTP。

```rust
use lellm_mcp::transport::{SseConfig, SseTransport};

let config = SseConfig::new("https://mcp.map.qq.com/sse?key=your_api_key&format=0")
    .with_request_timeout(std::time::Duration::from_secs(60));

let transport = SseTransport::new(config);
```

## QQ 地图 MCP 配置

### 获取 API Key

1. 访问 https://lbs.qq.com/service/webService/webServiceGuide/overview
2. 注册并创建 API Key
3. 开启 WebServiceAPI 功能

### 传输端点

- **Streamable HTTP（推荐）**: `https://mcp.map.qq.com/mcp?key=YOUR_KEY&format=0`
- **SSE（废弃，仅兼容）**: `https://mcp.map.qq.com/sse?key=YOUR_KEY&format=0`

### 参数说明

| 参数 | 必填 | 说明 |
|------|------|------|
| key | 是 | 开发者 API Key |
| format | 否 | 返回格式：0=语义化文本（默认），1=原始 JSON |

## 运行示例

### HTTP 示例（推荐）

```bash
TENCENT_MAP_KEY=your_api_key cargo run --example mcp_weather_http --features http
```

### SSE 示例（仅兼容旧服务器）

```bash
TENCENT_MAP_KEY=your_api_key cargo run --example mcp_weather_sse --features sse
```

## Feature Gates

在 `Cargo.toml` 中启用需要的 feature：

```toml
[dependencies]
lellm-mcp = { version = "0.4", features = ["http"] }     # Streamable HTTP（推荐）
lellm-mcp = { version = "0.4", features = ["sse"] }      # HTTP+SSE（已废弃，仅兼容）
lellm-mcp = { version = "0.4", features = ["http", "sse"] }  # 两者都启用
```

## 错误处理

| 错误类型 | 描述 | 处理方式 |
|---------|------|---------|
| `TransportError::Http` | HTTP 请求失败 | 检查网络和 URL |
| `TransportError::Timeout` | 请求超时 | 增加超时时间 |
| `TransportError::Disconnected` | 连接断开 | 重新连接 |
| `McpError::Protocol` | 协议错误 | 检查请求格式 |

## 性能优化

1. **连接池**：`reqwest::Client` 自动管理连接池
2. **超时设置**：根据网络情况调整超时时间
3. **无状态设计**：HttpTransport 天然支持负载均衡，无需会话亲和

## MCP 规范版本说明

| 规范版本 | 传输方式 | 状态 |
|---------|---------|------|
| 2024-11-05 | HTTP+SSE（双端点） | **Deprecated** |
| 2025-03-26 | Streamable HTTP（单端点） | Active |
| 2026-07-28 | Streamable HTTP（无状态，无 sessions） | **最新，Active** |

当前 lellm-mcp 的 HttpTransport 实现了 Streamable HTTP 的核心功能（单端点 POST + SSE 响应解析），
但尚未完全对齐 2026-07-28 规范的所有要求（如 `MCP-Protocol-Version` header、无状态模式等）。
详见 [[mcp-streamable-http-analysis]]。
