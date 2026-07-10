//! MCP 工具集成 — 将 MCP Server 暴露为 Agent 可消费的 ToolCatalog。
//!
//! 核心抽象：
//! - `CatalogStore` — 纯数据存储，使用 RwLock 实现读写分离
//! - `McpCatalog` — 纯读接口，供 Agent/ToolExecutor 使用
//! - `CatalogRefresher` — 纯写接口，供 Watcher 调用刷新
//! - `McpCatalogWatcher` — 后台监听 tools/list_changed，自动刷新
//! - `McpServerRegistry` — 多服务器管理，统一所有权模型

mod catalog;
mod registry;
mod watcher;

pub(crate) use catalog::CatalogRefresher;
pub(crate) use catalog::CatalogStore;
pub use catalog::McpCatalog;
pub use registry::{McpServerRegistry, ServerConfig};
pub use watcher::{CatalogRefresh, McpCatalogWatcher};

// Re-export ToolCatalog / ToolSnapshot from parent module for convenience
pub use super::{ToolCatalog, ToolSnapshot};
