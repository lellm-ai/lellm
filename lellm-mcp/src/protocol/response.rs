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
    /// 结果类型（MCP 2026-07-28+）。
    /// - `"complete"`: 普通结果
    /// - `"input_required"`: MRTR 中间结果，需要客户端提供额外输入
    #[serde(rename = "resultType", skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
}

impl CallToolResult {
    /// 检查结果是否为 MRTR 中间结果。
    pub fn is_input_required(&self) -> bool {
        self.result_type.as_deref() == Some("input_required")
    }
}

/// MRTR（Multi Round-Trip Requests）中间结果。
///
/// 服务器返回 `InputRequiredResult` 表示需要客户端提供额外输入。
/// 客户端应重试原请求，附带 `inputResponses`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRequiredResult {
    #[serde(rename = "resultType")]
    #[allow(dead_code)]
    pub result_type: String,
    #[serde(rename = "inputRequests")]
    #[allow(dead_code)]
    pub input_requests: Vec<InputRequest>,
}

/// MRTR 输入请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRequest {
    /// 请求类型（如 "elicitation/create", "sampling/createMessage"）
    #[allow(dead_code)]
    pub method: String,
    /// 请求参数
    #[allow(dead_code)]
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// 请求标识符，用于匹配响应
    #[allow(dead_code)]
    #[serde(rename = "requestId")]
    pub request_id: String,
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
