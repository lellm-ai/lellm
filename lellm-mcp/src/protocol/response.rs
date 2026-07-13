//! JSON-RPC Response + MCP 响应类型。

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 Response 结果（成功或错误）。
#[derive(Debug, Clone)]
pub enum JsonRpcResult {
    Success(serde_json::Value),
    Error(JsonRpcError),
}

impl Serialize for JsonRpcResult {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            JsonRpcResult::Success(value) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("result", value)?;
                map.end()
            }
            JsonRpcResult::Error(error) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("error", error)?;
                map.end()
            }
        }
    }
}

/// JSON-RPC 2.0 Error。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 Response。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(flatten)]
    pub result: JsonRpcResult,
}

// 自定义反序列化以区分 success/error
impl<'de> Deserialize<'de> for JsonRpcResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Helper {
            Success { result: serde_json::Value },
            Error { error: JsonRpcError },
        }

        let helper = Helper::deserialize(deserializer)?;
        Ok(match helper {
            Helper::Success { result } => JsonRpcResult::Success(result),
            Helper::Error { error } => JsonRpcResult::Error(error),
        })
    }
}

/// `initialize` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: serde_json::Value,
    #[serde(rename = "serverInfo")]
    pub server_info: ImplementationInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImplementationInfo {
    pub name: String,
    pub version: String,
}

/// `tools/list` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListToolsResult {
    pub tools: Vec<ToolInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// `tools/call` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(other)]
    Unknown,
}

impl ContentBlock {
    /// 提取文本内容。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }

    /// 将多个 ContentBlock 拼接为纯文本，忽略非 Text 类型。
    ///
    /// 语义：提取所有 Text 块，分隔符为 `\n\n`。
    /// 分隔符为 `\n\n`。当存在非文本块时发出 warn 日志。
    pub fn flatten_text(blocks: &[ContentBlock]) -> String {
        let has_non_text = blocks.iter().any(|b| b.as_text().is_none());
        if has_non_text {
            tracing::warn!(
                total = blocks.len(),
                "MCP tool returned non-text content blocks that will be dropped"
            );
        }
        blocks
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
