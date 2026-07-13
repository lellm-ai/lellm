//! lellm-mcp — MCP (Model Context Protocol) Client for LeLLM.
//!
//! 纯 MCP 协议实现，不包含任何 Agent/工具集成逻辑。
//!
//! 模块结构：
//! - `protocol` — JSON-RPC 消息类型
//! - `transport` — stdio / http / sse 传输层
//! - `client` — McpClient（连接管理 + 协议层）
//! - `server` — SimpleMcp（可选，server feature）

pub mod client;
pub mod protocol;
pub mod transport;

#[cfg(feature = "server")]
pub mod server;

pub use client::McpClient;
pub use protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError,
    RetryDisposition, TransportError,
};
pub use transport::{ConnectionState, McpTransport, TransportCapabilities};

#[cfg(feature = "server")]
pub use server::SimpleMcp;
