# MCP Transport 使用指南

> 版本：v0.1 | 日期：2026-07-03

## 概述

LeLLM 的 MCP 模块现在支持三种传输方式：

1. **StdioTransport** - 通过本地子进程通信
2. **SseTransport** - 通过 Server-Sent Events 通信
3. **HttpTransport** - 通过 Streamable HTTP 通信

## 传输方式对比

| 特性 | StdioTransport | SseTransport | HttpTransport |
|------|----------------|--------------|---------------|
| 连接方式 | 本地子进程 | 远程 SSE | 远程 HTTP |
| 依赖 | Node.js/Python | 无 | 无 |
| 延迟 | 高 | 低 | 低 |
| 稳定性 | 高 | 中 | 高 |
| 适用场景 | 本地 MCP 服务器 | 远程 MCP 服务器 | 远程 MCP 服务器 |

## 使用示例

### 1. StdioTransport（本地）

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

### 2. SseTransport（远程 SSE）

```rust
use lellm_mcp::transport::{SseConfig, SseTransport};

let config = SseConfig::new("https://mcp.map.qq.com/sse?key=your_api_key&format=0")
    .with_request_timeout(std::time::Duration::from_secs(60));

let transport = SseTransport::new(config);
```

### 3. HttpTransport（远程 HTTP）

```rust
use lellm_mcp::transport::{HttpConfig, HttpTransport};

let config = HttpConfig::new("https://mcp.map.qq.com/mcp?key=your_api_key&format=0")
    .with_request_timeout(std::time::Duration::from_secs(60));

let transport = HttpTransport::new(config);
```

## QQ 地图 MCP 配置

### 获取 API Key

1. 访问 https://lbs.qq.com/service/webService/webServiceGuide/overview
2. 注册并创建 API Key
3. 开启 WebServiceAPI 功能

### 传输端点

- **Streamable HTTP**: `https://mcp.map.qq.com/mcp?key=YOUR_KEY&format=0`
- **SSE**: `https://mcp.map.qq.com/sse?key=YOUR_KEY&format=0`

### 参数说明

| 参数 | 必填 | 说明 |
|------|------|------|
| key | 是 | 开发者 API Key |
| format | 否 | 返回格式：0=语义化文本（默认），1=原始 JSON |

## 运行示例

### SSE 示例

```bash
QQ_MAP_KEY=your_api_key cargo run --example mcp_weather --features sse
```

### HTTP 示例

```bash
QQ_MAP_KEY=your_api_key cargo run --example mcp_weather_http --features http
```

## Feature Gates

在 `Cargo.toml` 中启用需要的 feature：

```toml
[dependencies]
lellm-mcp = { version = "0.4", features = ["sse"] }  # 启用 SSE
# 或
lellm-mcp = { version = "0.4", features = ["http"] }  # 启用 HTTP
# 或
lellm-mcp = { version = "0.4", features = ["sse", "http"] }  # 启用两者
```

## 错误处理

| 错误类型 | 描述 | 处理方式 |
|---------|------|---------|
| `McpError::Network` | 网络连接失败 | 检查网络和 URL |
| `McpError::Timeout` | 请求超时 | 增加超时时间 |
| `McpError::Disconnected` | 连接断开 | 重新连接 |
| `McpError::Protocol` | 协议错误 | 检查请求格式 |

## 性能优化

1. **连接池**：`reqwest::Client` 自动管理连接池
2. **超时设置**：根据网络情况调整超时时间
3. **重连机制**：SSE 断开时自动重连
