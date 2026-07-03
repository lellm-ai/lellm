//! lellm-mcp — MCP (Model Context Protocol) Client for LeLLM.
//!
//! MVP 原型：验证接口冻结是否正确。
//!
//! 仅实现：stdio transport + tools/list + tools/call + initialize

pub mod protocol;
pub mod transport;

#[cfg(feature = "bridge")]
pub mod bridge;

#[cfg(feature = "bridge")]
pub mod client;

pub use protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError,
};
pub use transport::{ConnectionState, McpTransport};

#[cfg(feature = "bridge")]
pub use bridge::{McpCatalog, McpMultiClient, ToolCatalog};
#[cfg(feature = "bridge")]
pub use client::McpClient;
