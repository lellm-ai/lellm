---
name: mcp-server-feature-gate
description: MCP Server 代码通过 server feature gate 隔离，Client 用户不编译 Server
metadata:
  type: project
---

MCP Server 代码通过 `server` feature 隔离。仅使用 Client 的用户不编译 Server 代码（无 axum 依赖）。

**相关文件：**
- `lellm-mcp/Cargo.toml` — `server = ["dep:axum"]`
- `lellm-mcp/src/lib.rs` — `#[cfg(feature = "server")]` 保护 server 模块和 McpToolError
- `lellm-mcp/src/protocol/error.rs` — `McpToolError` 添加 `#[cfg(feature = "server")]`

**为什么 McpToolError 跟随 server：** 只在 SimpleMcp 工具函数返回类型中使用，Client 侧不需要。

参见 [[mcp-architecture]]。
