//! Multi-Server MCP Client — 管理多个 MCP 服务器连接。
//!
//! 参考 LangChain MultiServerMCPClient 设计：
//! - 接受多服务器配置
//! - 连接所有服务器，合并工具列表
//! - 实现 ToolCatalog，工具调用自动路由到对应服务器

use std::sync::Arc;

use indexmap::IndexMap;
use lellm_core::{ToolDefinition, ToolError, ToolErrorKind};

use super::{McpCatalog, ToolCatalog, ToolSnapshot};
use crate::client::McpClient;
use crate::protocol::{CallToolParams, JsonRpcRequest, methods};
use crate::transport::{
    HttpConfig, HttpTransport, SseConfig, SseTransport, StdioConfig, StdioTransport,
};

/// 服务器配置。
#[derive(Debug, Clone)]
pub enum ServerConfig {
    /// stdio 本地子进程
    Stdio {
        command: String,
        args: Vec<String>,
        env: Option<Vec<(String, String)>>,
    },
    /// SSE 远程连接
    Sse { url: String },
    /// HTTP 远程连接
    Http { url: String },
}

/// 多服务器 MCP Client。
///
/// 管理多个 MCP 服务器连接，合并工具列表，
/// 实现 ToolCatalog trait 供 Agent 使用。
pub struct McpMultiClient {
    /// 服务器名 → (McpClient, 工具列表)
    servers: IndexMap<String, ServerEntry>,
    version_counter: std::sync::atomic::AtomicU64,
}

struct ServerEntry {
    client: Arc<McpClient>,
    tools: IndexMap<String, ToolMeta>,
}

struct ToolMeta {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpMultiClient {
    /// 创建空的 MultiServerMCPClient。
    pub fn new() -> Self {
        Self {
            servers: IndexMap::new(),
            version_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 添加 stdio 服务器。
    pub async fn add_stdio(
        &mut self,
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<(), crate::McpError> {
        let config = StdioConfig::new(command, args).with_env(env);
        let transport = StdioTransport::new(config);
        let client = McpClient::with_transport(transport).await;
        self.connect_server(name.into(), client).await
    }

    /// 添加 SSE 服务器。
    pub async fn add_sse(
        &mut self,
        name: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<(), crate::McpError> {
        let config = SseConfig::new(url);
        let transport = SseTransport::new(config);
        let client = McpClient::with_transport(transport).await;
        self.connect_server(name.into(), client).await
    }

    /// 添加 HTTP 服务器。
    pub async fn add_http(
        &mut self,
        name: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<(), crate::McpError> {
        let config = HttpConfig::new(url);
        let transport = HttpTransport::new(config);
        let client = McpClient::with_transport(transport).await;
        self.connect_server(name.into(), client).await
    }

    /// 连接服务器并发现工具。
    async fn connect_server(
        &mut self,
        name: String,
        client: McpClient,
    ) -> Result<(), crate::McpError> {
        let client = Arc::new(client);

        // 连接 + 初始化
        client.connect().await?;
        client.initialize().await?;

        // 发现工具
        let catalog = McpCatalog::discover(client.clone()).await?;
        let snapshot = catalog.snapshot().await;

        let mut tools = IndexMap::new();
        for def in snapshot.definitions() {
            tools.insert(
                def.name.clone(),
                ToolMeta {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    input_schema: def.parameters.clone(),
                },
            );
        }

        tracing::info!(server = %name, tools = tools.len(), "Connected MCP server");

        self.servers.insert(name, ServerEntry { client, tools });
        Ok(())
    }

    /// 获取所有服务器的工具列表。
    pub fn tool_names(&self) -> Vec<(&str, Vec<&str>)> {
        self.servers
            .iter()
            .map(|(name, entry)| {
                (
                    name.as_str(),
                    entry.tools.keys().map(|s| s.as_str()).collect(),
                )
            })
            .collect()
    }

    /// 工具总数。
    pub fn total_tools(&self) -> usize {
        self.servers.values().map(|s| s.tools.len()).sum()
    }

    /// 通过工具名查找对应的 client。
    fn find_client(&self, tool_name: &str) -> Option<(&str, Arc<McpClient>)> {
        for (server_name, entry) in &self.servers {
            if entry.tools.contains_key(tool_name) {
                return Some((server_name, entry.client.clone()));
            }
        }
        None
    }

    /// 关闭所有连接。
    pub async fn close(&self) -> Result<(), crate::McpError> {
        for entry in self.servers.values() {
            entry.client.close().await?;
        }
        Ok(())
    }

    /// 发送 JSON-RPC 请求（自动路由到对应服务器）。
    pub async fn request(
        &self,
        req: JsonRpcRequest,
    ) -> Result<crate::protocol::JsonRpcResponse, crate::McpError> {
        // 从 params 中提取工具名
        let tool_name = req
            .params
            .as_ref()
            .and_then(|p| {
                p.get("name")
                    .or_else(|| p.get("tool"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");

        // 查找对应的 client
        let (_server_name, client) =
            self.find_client(tool_name)
                .ok_or(crate::McpError::ServerError(format!(
                    "tool '{}' not found in any server",
                    tool_name
                )))?;

        client.request(req).await
    }
}

impl Default for McpMultiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpMultiClient {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        let mut reg_map = IndexMap::new();

        for entry in self.servers.values() {
            for meta in entry.tools.values() {
                let client = entry.client.clone();
                let name = meta.name.clone();

                let def = ToolDefinition {
                    name: meta.name.clone(),
                    description: meta.description.clone(),
                    parameters: meta.input_schema.clone(),
                    cache_control: None,
                };

                let reg =
                    lellm_core::ExecutableTool::safe(def, move |input: &serde_json::Value| {
                        let client = client.clone();
                        let name = name.clone();
                        let input = input.clone();

                        async move {
                            let call_params = CallToolParams::new(&name, Some(input));
                            let params =
                                serde_json::to_value(&call_params).map_err(|e| ToolError {
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
                            Ok(serde_json::Value::String(text))
                        }
                    });

                reg_map.insert(meta.name.clone(), reg);
            }
        }

        let version = self
            .version_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .saturating_add(1);

        Arc::new(ToolSnapshot::new(reg_map, version))
    }
}
