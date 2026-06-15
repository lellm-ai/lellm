//! MCP Protocol — JSON-RPC 2.0 协议模型。
//!
//! 仅实现 v0.3 所需的方法：
//! - initialize
//! - tools/list
//! - tools/call
//! - notifications (initialized, tools/list_changed, progress)

mod error;
mod notification;
mod request;
mod response;

pub use error::McpError;
pub use notification::{JsonRpcNotification, NotificationKind, methods as notification_methods};
pub use request::{CallToolParams, ImplementationInfo, InitializeParams, JsonRpcRequest, methods};
pub use response::{
    CallToolResult, ContentBlock, InitializeResult, JsonRpcError, JsonRpcResponse, JsonRpcResult,
    ListToolsResult, ToolInfo,
};

/// JSON-RPC 2.0 Message（Request / Response / Notification）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

use serde::Deserialize;

impl JsonRpcMessage {
    /// 从 JSON 字符串解析。
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}
