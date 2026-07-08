//! lellm-mcp — MCP (Model Context Protocol) Client for LeLLM.
//!
//! MVP 原型：验证接口冻结是否正确。
//!
//! 仅实现：stdio transport + tools/list + tools/call + initialize

pub mod protocol;
pub mod transport;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "bridge")]
pub mod bridge;

#[cfg(feature = "bridge")]
pub mod client;

pub use protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError,
    RetryDisposition, TransportError,
};
pub use transport::{ConnectionState, McpTransport};

#[cfg(feature = "bridge")]
pub use bridge::{McpCatalog, McpCatalogWatcher, McpServerRegistry, ServerConfig, ToolCatalog};
#[cfg(feature = "bridge")]
pub use client::McpClient;
#[cfg(feature = "server")]
pub use server::SimpleMcp;
