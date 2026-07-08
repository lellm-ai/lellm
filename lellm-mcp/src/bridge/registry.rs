//! McpServerRegistry — 多服务器管理，替代 McpMultiClient。
//!
//! 与旧的 McpMultiClient 不同：
//! - 不再混在一起处理四种失效原因
//! - 实现 ToolCatalog，合并所有服务器的工具
//! - 提供 register() 返回 (client, watcher) 便于精细控制

use std::sync::Arc;

use indexmap::IndexMap;
use lellm_core::ToolDefinition;

use super::catalog::make_tool_entry;
use super::{ToolCatalog, ToolSnapshot};
use crate::client::McpClient;

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

/// 多服务器 MCP 注册表。
///
/// 管理多个 MCP 服务器连接，合并工具列表，
/// 实现 ToolCatalog trait 供 Agent 使用。
pub struct McpServerRegistry {
    servers: IndexMap<String, ServerEntry>,
}

struct ServerEntry {
    _client: Arc<McpClient>,
    tools: IndexMap<String, ToolDef>,
}

struct ToolDef {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpServerRegistry {
    /// 创建空的注册表。
    pub fn new() -> Self {
        Self {
            servers: IndexMap::new(),
        }
    }

    /// 注册一个已连接并初始化的服务器。
    ///
    /// 返回 (client, watcher) 便于精细控制：
    /// - `client` 可用于直接调用
    /// - `watcher` 可 spawn 后台自动刷新
    pub async fn register(
        &mut self,
        name: impl Into<String>,
        client: McpClient,
    ) -> Result<(Arc<McpClient>, super::watcher::McpCatalogWatcher), crate::McpError> {
        let name = name.into();
        let client_arc = Arc::new(client);

        // 发现工具
        let list_result: crate::protocol::ListToolsResult = client_arc.tools_list().await?;

        let mut tools = IndexMap::new();
        for tool in &list_result.tools {
            tools.insert(
                tool.name.clone(),
                ToolDef {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                },
            );
        }

        tracing::info!(server = %name, tools = tools.len(), "Registered MCP server");

        let client_for_entry = client_arc.clone();
        self.servers.insert(
            name,
            ServerEntry {
                _client: client_for_entry,
                tools,
            },
        );

        // 创建临时的 catalog 用于 watcher
        let catalog = super::catalog::McpCatalog::from_client(client_arc.clone()).await?;
        let catalog_arc = Arc::new(catalog);
        let watcher = super::watcher::McpCatalogWatcher::new(catalog_arc, &client_arc);

        Ok((client_arc, watcher))
    }

    /// 添加 stdio 服务器。
    #[cfg(feature = "stdio")]
    pub async fn add_stdio(
        &mut self,
        name: impl Into<String>,
        command: impl Into<String>,
        args: Vec<String>,
        env: Option<Vec<(String, String)>>,
    ) -> Result<(), crate::McpError> {
        use crate::transport::{StdioConfig, StdioTransport};
        let config = StdioConfig::new(command, args).with_env(env);
        let transport = StdioTransport::new(config);
        let mut client = McpClient::with_transport(transport);
        client.connect().await?;
        client.initialize().await?;
        let name_str = name.into();
        self.register(name_str, client).await.map(|_| ())
    }

    /// 添加 SSE 服务器。
    #[cfg(feature = "sse")]
    pub async fn add_sse(
        &mut self,
        name: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<(), crate::McpError> {
        use crate::transport::{SseConfig, SseTransport};
        let config = SseConfig::new(url);
        let transport = SseTransport::new(config);
        let mut client = McpClient::with_transport(transport);
        client.connect().await?;
        client.initialize().await?;
        let name_str = name.into();
        self.register(name_str, client).await.map(|_| ())
    }

    /// 添加 HTTP 服务器。
    #[cfg(feature = "http")]
    pub async fn add_http(
        &mut self,
        name: impl Into<String>,
        url: impl Into<String>,
    ) -> Result<(), crate::McpError> {
        use crate::transport::{HttpConfig, HttpTransport};
        let config = HttpConfig::new(url);
        let transport = HttpTransport::new(config);
        let mut client = McpClient::with_transport(transport);
        client.connect().await?;
        client.initialize().await?;
        let name_str = name.into();
        self.register(name_str, client).await.map(|_| ())
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
}

impl Default for McpServerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpServerRegistry {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        let mut reg_map = IndexMap::new();

        for entry in self.servers.values() {
            let client = entry._client.clone();
            for def in entry.tools.values() {
                reg_map.insert(
                    def.name.clone(),
                    make_tool_entry(
                        client.clone(),
                        def.name.clone(),
                        ToolDefinition {
                            name: def.name.clone(),
                            description: def.description.clone(),
                            parameters: def.input_schema.clone(),
                            cache_control: None,
                        },
                    ),
                );
            }
        }

        Arc::new(ToolSnapshot::new(reg_map, 0))
    }
}
