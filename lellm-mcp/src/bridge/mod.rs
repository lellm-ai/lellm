//! Tool 桥接 — MCP 工具 → ExecutableTool。
//!
//! 核心抽象：从 lellm-agent 复用 `ToolCatalog` trait。
//! - `CatalogStore` — 纯数据存储，使用 RwLock 实现读写分离
//! - `McpCatalog` — 纯读接口，供 Agent/ToolExecutor 使用
//! - `CatalogRefresher` — 纯写接口，供 Watcher 调用刷新
//! - `McpCatalogWatcher` — 后台监听 tools/list_changed，自动刷新
//! - `McpServerRegistry` — 多服务器管理，统一所有权模型

mod catalog;
mod registry;
mod watcher;

pub use catalog::{CatalogRefresher, CatalogStore, McpCatalog};
pub use registry::{McpServerRegistry, ServerConfig};
pub use watcher::{CatalogRefresh, McpCatalogWatcher};

// 从 lellm-agent 复用 ToolCatalog trait 和 ToolSnapshot
pub use lellm_agent::{ToolCatalog, ToolSnapshot};
