//! Tool 桥接 — MCP 工具 → ToolRegistration。
//!
//! 核心抽象：从 lellm-agent 复用 `ToolCatalog` trait。
//! McpCatalog 实现 ToolCatalog，通过 MCP 协议动态发现工具。

use std::sync::Arc;

use lellm_core::{ToolDefinition, ToolError, ToolErrorKind};

use super::client::McpClient;
use super::protocol::{CallToolParams, JsonRpcRequest, methods};

// 从 lellm-agent 复用 ToolCatalog trait，不重复定义
pub use lellm_agent::ToolCatalog;

/// MCP 工具目录 — 实现 lellm-agent 的 `ToolCatalog` trait。
pub struct McpCatalog {
    client: Arc<McpClient>,
    tools: Vec<McpToolEntry>,
}

struct McpToolEntry {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpCatalog {
    /// 从 MCP Client 发现工具。
    pub async fn discover(client: Arc<McpClient>) -> Result<Self, crate::McpError> {
        let resp = client
            .request(JsonRpcRequest::new(0, methods::TOOLS_LIST, None))
            .await?;

        let list_result: crate::protocol::ListToolsResult =
            serde_json::from_value(match &resp.result {
                crate::protocol::JsonRpcResult::Success(v) => v.clone(),
                crate::protocol::JsonRpcResult::Error(e) => {
                    return Err(crate::McpError::ServerError(e.message.clone()));
                }
            })
            .map_err(|e| crate::McpError::Protocol(e.to_string()))?;

        let tools = list_result
            .tools
            .into_iter()
            .map(|tool| McpToolEntry {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect();

        Ok(Self { client, tools })
    }

    /// 刷新工具列表（重新从 MCP Server 拉取）。
    pub async fn refresh(&mut self) -> Result<(), crate::McpError> {
        let resp = self
            .client
            .request(JsonRpcRequest::new(0, methods::TOOLS_LIST, None))
            .await?;

        let list_result: crate::protocol::ListToolsResult =
            serde_json::from_value(match &resp.result {
                crate::protocol::JsonRpcResult::Success(v) => v.clone(),
                crate::protocol::JsonRpcResult::Error(e) => {
                    return Err(crate::McpError::ServerError(e.message.clone()));
                }
            })
            .map_err(|e| crate::McpError::Protocol(e.to_string()))?;

        self.tools = list_result
            .tools
            .into_iter()
            .map(|tool| McpToolEntry {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect();

        Ok(())
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpCatalog {
    async fn snapshot(&self) -> Vec<lellm_agent::ToolRegistration> {
        self.tools
            .iter()
            .map(|entry| {
                let client = self.client.clone();
                let name = entry.name.clone();
                let args_schema = entry.input_schema.clone();

                let def = ToolDefinition {
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    parameters: entry.input_schema.clone(),
                };

                lellm_agent::ToolRegistration::safe(def, move |input: &serde_json::Value| {
                    let client = client.clone();
                    let name = name.clone();
                    let schema = args_schema.clone();

                    // 合并 input 和 schema defaults
                    let mut merged_args = serde_json::Map::new();
                    if let serde_json::Value::Object(defaults) = &schema {
                        merged_args.extend(defaults.clone());
                    }
                    if let serde_json::Value::Object(input_obj) = input {
                        merged_args.extend(input_obj.clone());
                    }
                    let final_args = serde_json::Value::Object(merged_args);

                    async move {
                        let call_params = CallToolParams::new(&name, Some(final_args));
                        let params = serde_json::to_value(&call_params)
                            .map_err(|e| ToolError {
                                kind: ToolErrorKind::Internal,
                                message: format!("serialize call params: {e}"),
                            })?;

                        let req = JsonRpcRequest::new(0, methods::TOOLS_CALL, Some(params));
                        let resp = client.request(req).await.map_err(|e| match e {
                            crate::McpError::Timeout => ToolError {
                                kind: ToolErrorKind::Timeout,
                                message: "mcp request timeout".to_string(),
                            },
                            crate::McpError::Disconnected => ToolError {
                                kind: ToolErrorKind::Network,
                                message: "mcp disconnected".to_string(),
                            },
                            e => ToolError {
                                kind: ToolErrorKind::Internal,
                                message: format!("mcp error: {e}"),
                            },
                        })?;

                        let call_result: crate::protocol::CallToolResult =
                            serde_json::from_value(match &resp.result {
                                crate::protocol::JsonRpcResult::Success(v) => v.clone(),
                                crate::protocol::JsonRpcResult::Error(e) => {
                                    return Err(ToolError {
                                        kind: ToolErrorKind::Internal,
                                        message: e.message.clone(),
                                    });
                                }
                            })
                            .map_err(|e| ToolError {
                                kind: ToolErrorKind::Internal,
                                message: format!("deserialize call result: {e}"),
                            })?;

                        if call_result.is_error {
                            let err_text = call_result
                                .content
                                .iter()
                                .filter_map(|c| c.as_text())
                                .collect::<Vec<_>>()
                                .join("\n");
                            return Err(ToolError {
                                kind: ToolErrorKind::Internal,
                                message: err_text,
                            });
                        }

                        let text = call_result
                            .content
                            .iter()
                            .filter_map(|c| c.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        Ok(text)
                    }
                })
            })
            .collect()
    }
}
