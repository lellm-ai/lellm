//! Tool 桥接 — MCP 工具 → ExecutableTool。
//!
//! 核心抽象：从 lellm-agent 复用 `ToolCatalog` trait。
//! McpCatalog 实现 ToolCatalog，通过 MCP 协议动态发现工具。
//! McpMultiClient 支持多服务器，合并工具列表。

mod multi;

pub use multi::{McpMultiClient, ServerConfig};

use std::sync::Arc;

use indexmap::IndexMap;
use lellm_core::{ToolDefinition, ToolError, ToolErrorKind};

use super::client::McpClient;
use super::protocol::{CallToolParams, JsonRpcRequest, methods};

// 从 lellm-agent 复用 ToolCatalog trait 和 ToolSnapshot
pub use lellm_agent::{ToolCatalog, ToolSnapshot};

/// MCP 工具目录 — 实现 lellm-agent 的 `ToolCatalog` trait。
///
/// 通过 MCP 协议动态发现工具，每次 snapshot() 调用时
/// 将已发现的工具集冻结为 ToolSnapshot。
pub struct McpCatalog {
    client: Arc<McpClient>,
    tools: IndexMap<String, McpToolEntry>,
    version_counter: std::sync::atomic::AtomicU64,
}

struct McpToolEntry {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpCatalog {
    /// 从 MCP Client 发现工具。
    pub async fn discover(client: Arc<McpClient>) -> Result<Self, crate::McpError> {
        let tools = Self::fetch_tools(&client).await?;
        Ok(Self {
            client,
            tools,
            version_counter: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// 刷新工具列表（重新从 MCP Server 拉取）。
    pub async fn refresh(&mut self) -> Result<(), crate::McpError> {
        self.tools = Self::fetch_tools(&self.client).await?;
        Ok(())
    }

    /// 从 MCP Server 拉取工具列表。
    async fn fetch_tools(
        client: &McpClient,
    ) -> Result<IndexMap<String, McpToolEntry>, crate::McpError> {
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

        Ok(list_result
            .tools
            .into_iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    McpToolEntry {
                        name: tool.name,
                        description: tool.description,
                        input_schema: tool.input_schema,
                    },
                )
            })
            .collect())
    }

    /// 工具数量。
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// 是否无工具。
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpCatalog {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        let mut reg_map = IndexMap::with_capacity(self.tools.len());

        for entry in self.tools.values() {
            let client = self.client.clone();
            let name = entry.name.clone();

            let def = ToolDefinition {
                name: entry.name.clone(),
                description: entry.description.clone(),
                parameters: entry.input_schema.clone(),
                cache_control: None,
            };

            let reg = lellm_core::ExecutableTool::safe(def, move |input: &serde_json::Value| {
                let client = client.clone();
                let name = name.clone();
                let input = input.clone();

                async move {
                    let call_params = CallToolParams::new(&name, Some(input));
                    let params = serde_json::to_value(&call_params).map_err(|e| ToolError {
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

                    // 提取文本内容，转为 serde_json::Value
                    let text = call_result
                        .content
                        .iter()
                        .filter_map(|c| c.as_text())
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(serde_json::Value::String(text))
                }
            });

            reg_map.insert(entry.name.clone(), reg);
        }

        let version = self
            .version_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);

        Arc::new(ToolSnapshot::new(reg_map, version))
    }
}
