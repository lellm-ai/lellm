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
    /// 在 params 中注入 `_meta`。
    ///
    /// 用于 MCP 2026-07-28 的无状态模式，每请求自包含协议版本和客户端信息。
    pub fn inject_meta(
        &mut self,
        protocol_version: impl Into<String>,
        client_info: Option<&ImplementationInfo>,
        client_capabilities: Option<serde_json::Value>,
    ) {
        let mut params = self.params.take().unwrap_or_else(|| serde_json::json!({}));
        let obj = params.as_object_mut().expect("params must be object");

        let mut meta = serde_json::Map::new();
        meta.insert(
            "io.modelcontextprotocol/protocolVersion".to_string(),
            serde_json::Value::String(protocol_version.into()),
        );

        if let Some(info) = client_info {
            meta.insert(
                "io.modelcontextprotocol/clientInfo".to_string(),
                serde_json::json!({ "name": info.name, "version": info.version }),
            );
        }

        if let Some(caps) = client_capabilities {
            meta.insert(
                "io.modelcontextprotocol/clientCapabilities".to_string(),
                caps,
            );
        }

        obj.insert("_meta".to_string(), serde_json::Value::Object(meta));

        self.params = Some(params);
    }
}

impl JsonRpcRequest {
    /// 构造一个 JSON-RPC Request。
    /// pub(crate) —— 只有 McpClient 能生成 request id。
    pub(crate) fn new(
        id: u64,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method_name: method.into(),
            params,
        }
    }

    /// 测试专用构造函数——仅供集成测试使用。
    pub fn new_for_test(
        id: u64,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self::new(id, method, params)
    }
}

/// MCP 方法名称常量。
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const TOOLS_LIST: &str = "tools/list";
    pub const TOOLS_CALL: &str = "tools/call";
    pub const PING: &str = "ping";
    pub const SERVER_DISCOVER: &str = "server/discover";
    pub const SUBSCRIPTIONS_LISTEN: &str = "subscriptions/listen";
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

/// `server/discover` 方法的参数。
///
/// 客户端用于发现服务器支持的协议版本、能力和身份信息。
/// 可选指定客户端支持的协议版本列表。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverParams {
    #[serde(rename = "protocolVersions", skip_serializing_if = "Option::is_none")]
    pub protocol_versions: Option<Vec<String>>,
}

impl DiscoverParams {
    pub fn new() -> Self {
        Self {
            protocol_versions: None,
        }
    }

    pub fn with_versions(mut self, versions: Vec<String>) -> Self {
        self.protocol_versions = Some(versions);
        self
    }
}

/// `server/discover` 方法的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryResult {
    #[serde(rename = "protocolVersions")]
    pub protocol_versions: Vec<String>,
    pub capabilities: serde_json::Value,
    #[serde(rename = "serverInfo")]
    pub server_info: ImplementationInfo,
}

/// `subscriptions/listen` 方法的参数。
///
/// 客户端订阅服务器发起的变更通知。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionsListenParams {
    #[serde(rename = "subscriptions")]
    pub subscriptions: Vec<String>,
}

impl SubscriptionsListenParams {
    pub fn new(subscriptions: Vec<String>) -> Self {
        Self { subscriptions }
    }

    /// 便捷构造：订阅工具列表变更
    pub fn tools_list_changed() -> Self {
        Self::new(vec!["toolsListChanged".to_string()])
    }

    /// 便捷构造：订阅资源列表变更
    pub fn resources_list_changed() -> Self {
        Self::new(vec!["resourcesListChanged".to_string()])
    }

    /// 便捷构造：订阅提示列表变更
    pub fn prompts_list_changed() -> Self {
        Self::new(vec!["promptsListChanged".to_string()])
    }
}
