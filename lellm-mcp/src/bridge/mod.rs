//! Tool 桥接 — MCP 工具 → ExecutableTool。
//!
//! 核心抽象：从 lellm-agent 复用 `ToolCatalog` trait。
//! - `McpCatalog` — 单服务器工具目录，原子更新快照
//! - `McpServerRegistry` — 多服务器管理，合并工具列表
//! - `McpCatalogWatcher` — 后台监听 tools/list_changed，自动刷新

mod catalog;
mod registry;
mod watcher;

pub use catalog::McpCatalog;
pub use registry::{McpServerRegistry, ServerConfig};
pub use watcher::McpCatalogWatcher;

// 从 lellm-agent 复用 ToolCatalog trait 和 ToolSnapshot
pub use lellm_agent::{ToolCatalog, ToolSnapshot};
