//! Catalog 模块 — 单 Server MCP 工具目录。
//!
//! `McpCatalog` 是 `ToolCatalog` 的实现之一，定位与 `StaticCatalog`、
//! `McpServerRegistry`、`CompositeCatalog` 平级：
//!
//! ```text
//! ToolCatalog
//!     ├── StaticCatalog        ← 本地静态工具
//!     ├── McpCatalog           ← 单个 MCP Server
//!     ├── McpServerRegistry    ← 多个 MCP Server
//!     └── CompositeCatalog     ← 任意 Catalog 的组合
//! ```
//!
//! 设计要点：
//! - `CatalogStore` — 纯数据存储（`pub(crate)`），使用 RwLock 实现读写分离
//! - `CatalogStoreWrite` — 写操作 trait，仅 `CatalogRefresher` 可见
//! - `McpCatalog` — 单 Server 目录，持有 Client 引用 + 工具快照
//! - `CatalogRefresher` — 纯写接口，供 Watcher 调用刷新

use std::sync::{Arc, RwLock, Weak};

use indexmap::IndexMap;
use lellm_core::{ToolDefinition, ToolError, ToolErrorKind};
use lellm_mcp::client::McpClient;

use super::watcher::CatalogRefresh;
use super::{ToolCatalog, ToolSnapshot};

// ─── CatalogStore — 纯数据存储（内部实现细节）────────────────────

/// 工具快照存储 — 零后台任务，纯数据容器。
///
/// 读方法直接在 impl 上，写方法收敛到 `CatalogStoreWrite` trait。
pub(crate) struct CatalogStore {
    snapshot: RwLock<Arc<ToolSnapshot>>,
}

impl CatalogStore {
    /// 创建存储。
    pub(crate) fn new(initial: Arc<ToolSnapshot>) -> Self {
        Self {
            snapshot: RwLock::new(initial),
        }
    }

    /// 加载当前快照（克隆 Arc，零锁竞争）。
    pub(crate) fn load(&self) -> Arc<ToolSnapshot> {
        self.snapshot.read().unwrap().clone()
    }

    /// 工具数量。
    pub(crate) fn len(&self) -> usize {
        self.snapshot.read().unwrap().len()
    }

    /// 是否无工具。
    pub(crate) fn is_empty(&self) -> bool {
        self.snapshot.read().unwrap().is_empty()
    }
}

/// CatalogStore 写操作 trait —— crate 内部可见，`McpCatalog` 无法暴露写入能力。
pub(crate) trait CatalogStoreWrite {
    /// 直接存储快照。
    fn store_raw(&self, snapshot: Arc<ToolSnapshot>);
}

impl CatalogStoreWrite for CatalogStore {
    fn store_raw(&self, snapshot: Arc<ToolSnapshot>) {
        *self.snapshot.write().unwrap() = snapshot;
    }
}

// ─── McpCatalog — 单 Server 工具目录 ─────────────────────────────

/// 单个 MCP Server 的工具目录。
///
/// 与 `StaticCatalog`、`McpServerRegistry`、`CompositeCatalog` 平级，
/// 都是 `ToolCatalog` 的实现。适用于只需要连接一个 MCP Server 的场景。
///
/// # 示例
///
/// ```rust,ignore
/// let client = McpClient::connect_stdio(cmd).await?;
/// let catalog = McpCatalog::discover(client.into()).await?;
///
/// let agent = AgentBuilder::new(model)
///     .catalog("my-mcp", Arc::new(catalog))
///     .build();
/// ```
pub struct McpCatalog {
    client: Arc<McpClient>,
    store: Arc<CatalogStore>,
}

impl McpCatalog {
    /// 通过 MCP `tools/list` 发现工具，创建目录。
    ///
    /// 执行一次 `tools/list` 远程调用，构建工具快照。
    /// 返回的 `McpCatalog` 持有 `client` 引用，保持 Transport 存活。
    pub async fn discover(client: Arc<McpClient>) -> Result<Self, lellm_mcp::McpError> {
        let snapshot = build_snapshot(client.clone()).await?;
        Ok(Self {
            client,
            store: Arc::new(CatalogStore::new(snapshot)),
        })
    }

    /// 获取持有的 MCP Client 引用。
    ///
    /// 用于后续扩展（如 Prompt、Resource、Sampling 等需要 Client 的场景）。
    pub fn client(&self) -> Arc<McpClient> {
        Arc::clone(&self.client)
    }

    /// 创建工具目录刷新器 — 返回 `Arc<dyn CatalogRefresh>`，隐藏内部实现。
    pub fn create_refresher(&self) -> Arc<dyn CatalogRefresh> {
        Arc::new(CatalogRefresher::new(
            Arc::clone(&self.client),
            Arc::clone(&self.store),
        ))
    }

    /// 读取当前快照。
    pub fn load_full(&self) -> Arc<ToolSnapshot> {
        self.store.load()
    }

    /// 工具数量。
    pub fn len(&self) -> usize {
        self.store.len()
    }

    /// 是否无工具。
    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    /// 拆分为内部组件（供 `McpServerRegistry` 内部使用）。
    pub(crate) fn into_parts(self) -> (Arc<McpClient>, Arc<CatalogStore>) {
        (self.client, self.store)
    }
}

impl Clone for McpCatalog {
    fn clone(&self) -> Self {
        Self {
            client: Arc::clone(&self.client),
            store: Arc::clone(&self.store),
        }
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpCatalog {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        self.store.load()
    }
}

// ─── CatalogRefresher — 纯写接口（内部实现细节）──────────────────

/// 工具目录刷新器 — 持有 Client（Weak）和 Store，执行 refresh 操作。
///
/// 使用 `Weak<McpClient>` 避免 Watcher 反向延长 Client 的生命周期。
/// 当 Client 被移除（`Registry::remove()`），Watcher 下一次刷新时自动退出。
pub(crate) struct CatalogRefresher {
    client: Weak<McpClient>,
    store: Arc<CatalogStore>,
}

impl CatalogRefresher {
    /// 创建刷新器。
    pub(crate) fn new(client: Arc<McpClient>, store: Arc<CatalogStore>) -> Self {
        Self {
            client: Arc::downgrade(&client),
            store,
        }
    }

    /// 刷新工具目录 — 拉取最新工具列表并更新 Store。
    ///
    /// 如果 Client 已被 Drop（服务器被移除），返回 `McpError::Transport(Disconnected)`。
    pub(crate) async fn refresh_impl(&self) -> Result<(), lellm_mcp::McpError> {
        use CatalogStoreWrite;
        let client = self
            .client
            .upgrade()
            .ok_or_else(lellm_mcp::McpError::disconnected)?;
        let new_snapshot = build_snapshot(client).await?;
        self.store.store_raw(new_snapshot);
        Ok(())
    }
}

/// 为 CatalogRefresher 实现 CatalogRefresh trait。
/// Watcher 通过此 trait 触发刷新，不依赖具体实现。
#[async_trait::async_trait]
impl CatalogRefresh for CatalogRefresher {
    async fn refresh(&self) -> Result<(), lellm_mcp::McpError> {
        self.refresh_impl().await
    }
}

// ─── 共享工具函数 ────────────────────────────────────────────────

/// 构建工具快照。
pub(super) async fn build_snapshot(
    client: Arc<McpClient>,
) -> Result<Arc<ToolSnapshot>, lellm_mcp::McpError> {
    let list_result: lellm_mcp::protocol::ListToolsResult = client.tools_list().await?;

    let mut reg_map = IndexMap::with_capacity(list_result.tools.len());
    for tool in &list_result.tools {
        let entry = make_tool_entry(
            client.clone(),
            tool.name.clone(),
            ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: lellm_core::ToolSchema::new(tool.input_schema.clone()),
                cache_control: None,
            },
        );
        reg_map.insert(tool.name.clone(), entry);
    }

    Ok(Arc::new(ToolSnapshot::new(reg_map)))
}

/// 将 MCP 工具转换为 ExecutableTool。
pub(super) fn make_tool_entry(
    client: Arc<McpClient>,
    name: String,
    def: ToolDefinition,
) -> lellm_core::ExecutableTool {
    let name_clone = name.clone();

    lellm_core::ExecutableTool::safe(def, move |input: &serde_json::Value| {
        let client = client.clone();
        let name = name_clone.clone();
        let input = input.clone();

        async move {
            let call_result: lellm_mcp::protocol::CallToolResult = client
                .call_tool(&name, Some(input.clone()))
                .await
                .map_err(|e| mcp_error_to_tool_error(&e))?;

            if call_result.is_error {
                let err_text =
                    lellm_mcp::protocol::ContentBlock::flatten_text(&call_result.content);
                return Err(ToolError {
                    kind: ToolErrorKind::Internal,
                    message: err_text,
                });
            }

            let text = lellm_mcp::protocol::ContentBlock::flatten_text(&call_result.content);
            Ok(serde_json::Value::String(text))
        }
    })
}

/// 将 McpError 映射为 ToolError。
pub(super) fn mcp_error_to_tool_error(e: &lellm_mcp::McpError) -> ToolError {
    match e {
        lellm_mcp::McpError::Transport(lellm_mcp::protocol::TransportError::Timeout) => ToolError {
            kind: ToolErrorKind::Timeout,
            message: "mcp request timeout".to_string(),
        },
        lellm_mcp::McpError::Transport(lellm_mcp::protocol::TransportError::Disconnected) => {
            ToolError {
                kind: ToolErrorKind::Network,
                message: "mcp disconnected".to_string(),
            }
        }
        _ => ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("mcp error: {e}"),
        },
    }
}
