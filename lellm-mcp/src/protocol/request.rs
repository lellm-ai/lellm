//! JSON-RPC Request + MCP 方法定义。

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 Request。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(rename = "method")]
    pub method_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    /// 构造一个 JSON-RPC Request。
    pub fn new(id: u64, method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method_name: method.into(),
            params,
        }
    }
}

/// MCP 方法名称常量。
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const PING: &str = "ping";
}

/// `initialize` 方法的参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: serde_json::Value,
    #[serde(rename = "clientInfo", skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ImplementationInfo>,
}

impl InitializeParams {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            protocol_version: version.into(),
            capabilities: serde_json::json!({}),
            client_info: None,
        }
    }

    pub fn with_client_info(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.client_info = Some(ImplementationInfo {
            name: name.into(),
            version: version.into(),
        });
        self
    }
}

/// 客户端/服务端实现信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationInfo {
    pub name: String,
    pub version: String,
}

/// `tools/call` 方法的参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolParams {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

impl CallToolParams {
    pub fn new(name: impl Into<String>, arguments: Option<serde_json::Value>) -> Self {
        Self {
            name: name.into(),
            arguments,
        }
    }
}
