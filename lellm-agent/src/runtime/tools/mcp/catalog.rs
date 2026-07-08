//! Catalog 模块 — 读写分离的工具目录。
//!
//! 设计要点：
//! - `CatalogStore` — 纯数据存储，使用 RwLock 实现读写分离
//! - `McpCatalog` — 纯读接口，供 Agent/ToolExecutor 使用
//! - `CatalogRefresher` — 纯写接口，供 Watcher 调用刷新

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use lellm_core::{ToolDefinition, ToolError, ToolErrorKind};
use lellm_mcp::client::McpClient;

use super::{ToolCatalog, ToolSnapshot};

// ─── CatalogStore — 纯数据存储 ──────────────────────────────────

/// 工具快照存储 — 零后台任务，纯数据容器。
///
/// 维护单调递增的版本计数器（对齐 `CompositeCatalog` 模式），
/// 每次 `next_version()` 自增，`store_raw()` 直接替换快照。
pub struct CatalogStore {
    snapshot: RwLock<Arc<ToolSnapshot>>,
    version: AtomicU64,
}

impl CatalogStore {
    /// 创建存储，记录初始快照的版本号。
    pub fn new(initial: Arc<ToolSnapshot>) -> Self {
        let initial_version = initial.version();
        Self {
            snapshot: RwLock::new(initial),
            version: AtomicU64::new(initial_version),
        }
    }

    /// 自增版本号并返回新值——供 `CatalogRefresher` 在 build_snapshot 时使用。
    pub fn next_version(&self) -> u64 {
        self.version.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// 直接存储快照（不修改版本号）——版本号由调用方在 build_snapshot 时传入。
    pub fn store_raw(&self, snapshot: Arc<ToolSnapshot>) {
        *self.snapshot.write().unwrap() = snapshot;
    }

    /// 加载当前快照（克隆 Arc，零锁竞争）。
    pub fn load(&self) -> Arc<ToolSnapshot> {
        self.snapshot.read().unwrap().clone()
    }

    /// 工具数量。
    pub fn len(&self) -> usize {
        self.snapshot.read().unwrap().len()
    }

    /// 是否无工具。
    pub fn is_empty(&self) -> bool {
        self.snapshot.read().unwrap().is_empty()
    }
}

// ─── McpCatalog — 纯读接口 ─────────────────────────────────────

/// MCP 工具目录 — 纯读接口，供 Agent/ToolExecutor 使用。
pub struct McpCatalog {
    store: Arc<CatalogStore>,
}

impl McpCatalog {
    /// 从 MCP Client 发现工具，创建目录。
    pub async fn from_client(client: Arc<McpClient>) -> Result<Self, lellm_mcp::McpError> {
        let snapshot = build_snapshot(client, 0).await?;
        Ok(Self {
            store: Arc::new(CatalogStore::new(snapshot)),
        })
    }

    /// 从 CatalogStore 创建（供 Registry 内部使用）。
    pub fn from_store(store: Arc<CatalogStore>) -> Self {
        Self { store }
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

    /// 获取底层 CatalogStore（供 CatalogRefresher 使用）。
    pub fn store(&self) -> &Arc<CatalogStore> {
        &self.store
    }
}

impl Clone for McpCatalog {
    fn clone(&self) -> Self {
        Self {
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

// ─── CatalogRefresher — 纯写接口 ────────────────────────────────

/// 工具目录刷新器 — 持有 Client 和 Store，执行 refresh 操作。
pub struct CatalogRefresher {
    client: Arc<McpClient>,
    store: Arc<CatalogStore>,
}

impl CatalogRefresher {
    /// 创建刷新器。
    pub fn new(client: Arc<McpClient>, store: Arc<CatalogStore>) -> Self {
        Self { client, store }
    }

    /// 从 Client 和 Catalog 创建。
    pub fn from_catalog(client: Arc<McpClient>, catalog: &McpCatalog) -> Self {
        Self {
            client,
            store: Arc::clone(&catalog.store),
        }
    }

    /// 刷新工具目录 — 拉取最新工具列表并更新 Store。
    pub async fn refresh_impl(&self) -> Result<(), lellm_mcp::McpError> {
        let version = self.store.next_version();
        let new_snapshot = build_snapshot(self.client.clone(), version).await?;
        self.store.store_raw(new_snapshot);
        Ok(())
    }
}

// ─── 共享工具函数 ────────────────────────────────────────────────

/// 构建工具快照。
async fn build_snapshot(
    client: Arc<McpClient>,
    version: u64,
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
                parameters: tool.input_schema.clone(),
                cache_control: None,
            },
        );
        reg_map.insert(tool.name.clone(), entry);
    }

    Ok(Arc::new(ToolSnapshot::new(reg_map, version)))
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
