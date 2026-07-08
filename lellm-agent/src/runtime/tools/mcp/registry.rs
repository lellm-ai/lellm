//! McpServerRegistry — 多服务器管理，统一所有权模型。
//!
//! 设计要点：
//! - Registry 是所有后台任务的唯一 Owner
//! - ManagedServer 封装单个服务器的所有资源
//! - Drop Registry 时自动 cancel + join 所有后台任务
//! - 用户通过 `register(name, client)` 注册已连接的客户端

use std::sync::Arc;

use indexmap::IndexMap;
use tokio_util::sync::CancellationToken;

use super::catalog::{CatalogRefresher, CatalogStore, McpCatalog};
use super::watcher::McpCatalogWatcher;
use super::{ToolCatalog, ToolSnapshot};
use lellm_mcp::client::McpClient;

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

/// 受管理的服务器实例 — 封装单个服务器的所有资源。
struct ManagedServer {
    /// MCP Client — 保持 Transport 存活。
    _client: Arc<McpClient>,
    /// 工具快照存储。
    store: Arc<CatalogStore>,
    /// 取消令牌 — 用于停止后台任务。
    cancel: CancellationToken,
    /// Watcher 的 JoinHandle — Transport 不支持 notifications 时为 None。
    watcher: Option<tokio::task::JoinHandle<()>>,
}

/// 多服务器 MCP 注册表。
///
/// 管理多个 MCP 服务器连接，合并工具列表，
/// 实现 ToolCatalog trait 供 Agent 使用。
///
/// 所有权模型：
/// - Registry 拥有所有 ManagedServer
/// - ManagedServer 拥有 client、store、cancel token、watcher handle
/// - Drop Registry 时自动 cancel + join 所有后台任务
pub struct McpServerRegistry {
    servers: IndexMap<String, ManagedServer>,
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
    /// 自动启动 Watcher 监听 tools/list_changed 通知。
    /// 返回 McpCatalog 供 Agent 使用。
    pub async fn register(
        &mut self,
        name: impl Into<String>,
        client: McpClient,
    ) -> Result<McpCatalog, lellm_mcp::McpError> {
        let name = name.into();
        let client_arc = Arc::new(client);

        // 发现工具并创建 Catalog
        let catalog = McpCatalog::from_client(client_arc.clone()).await?;

        // 创建取消令牌
        let cancel = CancellationToken::new();

        // 仅在 Transport 支持 notifications 时 spawn Watcher
        let watcher = match client_arc.subscribe_notifications() {
            Some(rx) => {
                let refresher =
                    Arc::new(CatalogRefresher::from_catalog(client_arc.clone(), &catalog));
                Some(McpCatalogWatcher::new(refresher, rx).spawn(cancel.clone()))
            }
            None => {
                tracing::debug!(
                    server = %name,
                    "transport does not support notifications, skipping watcher"
                );
                None
            }
        };

        tracing::info!(
            server = %name,
            tools = catalog.len(),
            "Registered MCP server"
        );

        self.servers.insert(
            name,
            ManagedServer {
                _client: client_arc,
                store: Arc::clone(catalog.store()),
                cancel,
                watcher,
            },
        );

        Ok(catalog)
    }

    /// 获取所有服务器的工具列表。
    pub fn tool_names(&self) -> Vec<(&str, Vec<String>)> {
        self.servers
            .iter()
            .map(|(name, entry)| {
                let snapshot = entry.store.load();
                let tool_names: Vec<String> = snapshot
                    .definitions()
                    .iter()
                    .map(|d| d.name.clone())
                    .collect();
                (name.as_str(), tool_names)
            })
            .collect()
    }

    /// 工具总数。
    pub fn total_tools(&self) -> usize {
        self.servers.values().map(|s| s.store.len()).sum()
    }

    /// 获取指定服务器的 CatalogStore（用于调试）。
    pub fn store(&self, name: &str) -> Option<Arc<CatalogStore>> {
        self.servers.get(name).map(|s| Arc::clone(&s.store))
    }

    /// 优雅关闭所有服务器后台任务。
    ///
    /// 1. 发送 cancel 信号，等待 Watcher 自然退出
    /// 2. 超时（2s）后 abort 仍未退出的 Watcher
    ///
    /// 与 `Drop` 的区别：
    /// - `shutdown()` 是异步的，先尝试优雅关闭
    /// - `Drop` 是同步兜底，直接 abort 所有任务
    pub async fn shutdown(&mut self) {
        // 第一步：发送 cancel 信号
        for entry in self.servers.values() {
            entry.cancel.cancel();
        }

        // 第二步：等待所有 watcher 自然退出（带超时）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        for entry in self.servers.values_mut() {
            if let Some(ref mut watcher) = entry.watcher {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let _ = tokio::time::timeout(remaining, watcher).await;
            }
        }

        // 第三步：abort 仍未退出的 watcher
        for entry in self.servers.values_mut() {
            if let Some(watcher) = entry.watcher.as_mut() {
                watcher.abort();
            }
        }

        // 第四步：等待 abort 完成（短暂等待）
        for entry in self.servers.values_mut() {
            if let Some(watcher) = entry.watcher.as_mut() {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(100), watcher).await;
            }
        }
    }
}

impl Default for McpServerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for McpServerRegistry {
    fn drop(&mut self) {
        // 兜底关闭——直接 abort 所有后台任务。
        // 优雅关闭请使用 async `shutdown()` 方法。
        // 注意：Drop 是同步的，不能调用 async 方法。
        // JoinHandle drop 会 detach 任务（不阻塞），abort 确保任务终止。
        for entry in self.servers.values_mut() {
            if let Some(watcher) = entry.watcher.as_mut() {
                watcher.abort();
            }
        }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpServerRegistry {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        let mut reg_map = IndexMap::new();
        let mut max_version = 0u64;

        for (server_name, entry) in &self.servers {
            let snapshot = entry.store.load();
            // 跟踪最大版本号
            max_version = max_version.max(snapshot.version());
            // 直接迭代 (name, ExecutableTool)，零哈希查找
            for (tool_name, tool) in snapshot.iter() {
                if reg_map.contains_key(tool_name) {
                    tracing::warn!(
                        tool = %tool_name,
                        server = %server_name,
                        "MCP tool name conflict — later server shadows earlier one"
                    );
                }
                reg_map.insert(tool_name.to_string(), tool.clone());
            }
        }

        Arc::new(ToolSnapshot::new(reg_map, max_version))
    }
}
