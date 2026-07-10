//! McpServerRegistry — 多服务器管理，统一所有权模型。
//!
//! 设计要点：
//! - Registry 是所有后台任务的唯一 Owner
//! - ManagedServer 封装单个服务器的所有资源
//! - Drop Registry 时自动 cancel + join 所有后台任务
//! - 工具名冲突默认 Fail-Fast（注册时报错）
//! - 可选 Prefix / Override / Custom 策略

use std::sync::Arc;

use indexmap::IndexMap;
use tokio_util::sync::CancellationToken;

use super::catalog::{CatalogRefresher, CatalogStore, McpCatalog};
use super::watcher::McpCatalogWatcher;
use super::{ToolCatalog, ToolSnapshot};
use lellm_core::ExecutableTool;
use lellm_mcp::client::McpClient;

// ─── 冲突策略 ────────────────────────────────────────────────────

/// 工具名冲突错误。
#[derive(Debug, Clone)]
pub struct NameConflictError {
    /// 冲突的工具名。
    pub tool_name: String,
    /// 已注册的服务器名。
    pub existing_server: String,
    /// 新注册的服务器名。
    pub new_server: String,
}

impl std::fmt::Display for NameConflictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tool name conflict: \"{}\" already registered by server \"{}\", \
             attempted by server \"{}\"",
            self.tool_name, self.existing_server, self.new_server
        )
    }
}

impl std::error::Error for NameConflictError {}

/// 工具名冲突解决策略。
///
/// 默认 `Error`（Fail-Fast）——注册时立即报错，不静默覆盖。
#[derive(Debug, Clone, Default)]
pub enum NameConflictPolicy {
    /// 注册时检测到冲突立即返回 `RegistryError::NameConflict`。
    /// **默认策略**。
    #[default]
    Error,
    /// 后注册的服务器覆盖先注册的（原行为）。
    /// ⚠️ 生产环境慎用——配置错误不会立即暴露。
    Override,
    /// 使用 `{server_name}{separator}{tool_name}` 作为注册名。
    /// 在 `snapshot()` 合并时应用前缀，不修改原始 ToolDefinition。
    Prefix { separator: String },
}

/// Registry 操作错误。
#[derive(Debug)]
pub enum RegistryError {
    /// 工具名冲突。
    NameConflict(NameConflictError),
    /// MCP 协议错误。
    Mcp(lellm_mcp::McpError),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::NameConflict(e) => write!(f, "{e}"),
            RegistryError::Mcp(e) => write!(f, "mcp error: {e}"),
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegistryError::NameConflict(e) => Some(e),
            RegistryError::Mcp(e) => Some(e),
        }
    }
}

impl From<lellm_mcp::McpError> for RegistryError {
    fn from(e: lellm_mcp::McpError) -> Self {
        RegistryError::Mcp(e)
    }
}

// ─── 服务器配置 ──────────────────────────────────────────────────

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

// ─── 内部结构 ────────────────────────────────────────────────────

/// 受管理的服务器实例 — 封装单个服务器的所有资源。
struct ManagedServer {
    /// MCP Client — 持有 Arc 保持 Transport 存活。
    /// 如果不持有，McpClient 可能被 Drop → Transport 关闭。
    client: Arc<McpClient>,
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
    /// 工具名 → 服务器名索引，用于 Fail-Fast 冲突检测。
    tool_index: IndexMap<String, String>,
    /// 冲突解决策略。
    conflict_policy: NameConflictPolicy,
}

impl McpServerRegistry {
    /// 创建空的注册表，使用默认冲突策略（`Error`）。
    pub fn new() -> Self {
        Self {
            servers: IndexMap::new(),
            tool_index: IndexMap::new(),
            conflict_policy: NameConflictPolicy::default(),
        }
    }

    /// 设置冲突解决策略。
    pub fn set_conflict_policy(&mut self, policy: NameConflictPolicy) {
        self.conflict_policy = policy;
    }

    /// 获取当前冲突策略的引用。
    pub fn conflict_policy(&self) -> &NameConflictPolicy {
        &self.conflict_policy
    }

    /// 注册一个已连接并初始化的服务器。
    ///
    /// 自动启动 Watcher 监听 tools/list_changed 通知。
    /// 返回 McpCatalog 供 Agent 使用。
    ///
    /// # 错误
    /// - `RegistryError::NameConflict` — 工具名冲突且策略为 `Error`
    /// - `RegistryError::Mcp` — MCP 协议错误
    pub async fn register(
        &mut self,
        name: impl Into<String>,
        client: McpClient,
    ) -> Result<McpCatalog, RegistryError> {
        let name = name.into();
        let client_arc = Arc::new(client);

        // 发现工具并创建共享的 CatalogStore
        let snapshot = super::catalog::build_snapshot(client_arc.clone(), 0).await?;

        // 冲突检测
        self.check_conflicts(&name, snapshot.definitions())?;

        // 注册工具名到全局索引（Error 策略）
        self.register_tool_names(&name, snapshot.definitions());

        let store = Arc::new(CatalogStore::new(snapshot));
        let catalog = McpCatalog::from_store(Arc::clone(&store));

        // 创建取消令牌
        let cancel = CancellationToken::new();

        // 仅在 Transport 支持 notifications 时 spawn Watcher
        let watcher = match client_arc.subscribe_notifications() {
            Some(rx) => {
                let refresher = Arc::new(CatalogRefresher::new(
                    client_arc.clone(),
                    Arc::clone(&store),
                ));
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
                client: client_arc,
                store,
                cancel,
                watcher,
            },
        );

        Ok(catalog)
    }

    /// 检查工具名冲突。
    ///
    /// `Error` 策略下，注册时立即报错；
    /// `Override` / `Prefix` 策略下，不报错（snapshot 合并时处理）。
    fn check_conflicts(
        &self,
        server_name: &str,
        definitions: &[lellm_core::ToolDefinition],
    ) -> Result<(), RegistryError> {
        if !matches!(&self.conflict_policy, NameConflictPolicy::Error) {
            return Ok(());
        }

        for def in definitions {
            if let Some(existing) = self.tool_index.get(&def.name) {
                return Err(RegistryError::NameConflict(NameConflictError {
                    tool_name: def.name.clone(),
                    existing_server: existing.clone(),
                    new_server: server_name.to_string(),
                }));
            }
        }
        Ok(())
    }

    /// 将工具名注册到全局索引（Error 策略使用）。
    fn register_tool_names(
        &mut self,
        server_name: &str,
        definitions: &[lellm_core::ToolDefinition],
    ) {
        for def in definitions {
            self.tool_index
                .insert(def.name.clone(), server_name.to_string());
        }
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
    pub(crate) fn store(&self, name: &str) -> Option<Arc<CatalogStore>> {
        self.servers.get(name).map(|s| Arc::clone(&s.store))
    }

    /// 优雅关闭所有服务器后台任务。
    ///
    /// 每个 server 独立处理：cancel → 等待 → abort（不互相影响）
    /// 然后短暂等待 abort 完成。
    ///
    /// 与 `Drop` 的区别：
    /// - `shutdown()` 是异步的，先尝试优雅关闭
    /// - `Drop` 是同步兜底，直接 abort 所有任务
    pub async fn shutdown(&mut self) {
        // 第 1 遍：每个 server 独立 cancel → 等待 → abort
        for entry in self.servers.values_mut() {
            entry.cancel.cancel();
            if let Some(ref mut watcher) = entry.watcher {
                let _ = tokio::time::timeout(std::time::Duration::from_millis(500), &mut *watcher)
                    .await;
                if !watcher.is_finished() {
                    watcher.abort();
                }
            }
        }

        // 第 2 遍：短暂等待 abort 完成
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
            max_version = max_version.max(snapshot.version());

            for (tool_name, tool) in snapshot.iter() {
                match &self.conflict_policy {
                    NameConflictPolicy::Error => {
                        // Error 模式下注册时已检查，直接插入
                        reg_map.insert(tool_name.to_string(), tool.clone());
                    }
                    NameConflictPolicy::Override => {
                        if reg_map.contains_key(tool_name) {
                            tracing::warn!(
                                tool = %tool_name,
                                server = %server_name,
                                "MCP tool name conflict — later server shadows earlier one"
                            );
                        }
                        reg_map.insert(tool_name.to_string(), tool.clone());
                    }
                    NameConflictPolicy::Prefix { separator } => {
                        let prefixed = format!("{server_name}{separator}{tool_name}");
                        // 创建带前缀名的新 ExecutableTool
                        let prefixed_tool = create_prefixed_tool(&prefixed, tool);
                        reg_map.insert(prefixed, prefixed_tool);
                    }
                }
            }
        }

        Arc::new(ToolSnapshot::new(reg_map, max_version))
    }
}

/// 创建带前缀名的 ExecutableTool 包装。
///
/// 新的 ToolDefinition 使用 `prefixed_name`，
/// 但内部闭包仍然调用原始工具（使用原始 MCP 工具名）。
fn create_prefixed_tool(prefixed_name: &str, original: &ExecutableTool) -> ExecutableTool {
    let original = original.clone();
    ExecutableTool::safe(
        lellm_core::ToolDefinition {
            name: prefixed_name.to_string(),
            description: original.definition.description.clone(),
            parameters: original.definition.parameters.clone(),
            cache_control: original.definition.cache_control.clone(),
        },
        move |input: &serde_json::Value| {
            let orig = original.clone();
            let input = input.clone();
            async move { orig.execute(&input).await }
        },
    )
}
