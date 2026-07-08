//! McpCatalog — 工具目录，使用 RwLock 原子更新快照。
//!
//! 设计要点：
//! - 内部 `RwLock<Arc<ToolSnapshot>>` 实现读写分离
//! - 持有 `Arc<McpClient>` 保持 Transport 存活（工具闭包共享同一 Arc）
//! - `snapshot()` 获取读锁返回当前快照
//! - `update_tools()` 在网络调用外不持有写锁（仅在 store 时短暂持有）

use std::sync::{Arc, RwLock};

use indexmap::IndexMap;
use lellm_core::{ToolDefinition, ToolError, ToolErrorKind};

use super::{ToolCatalog, ToolSnapshot};
use crate::client::McpClient;

/// MCP 工具目录 — RwLock 保护的可更新快照。
pub struct McpCatalog {
    snapshot: RwLock<Arc<ToolSnapshot>>,
    client: Arc<McpClient>,
}

impl McpCatalog {
    /// 从 MCP Client 发现工具，创建目录。
    pub async fn from_client(client: Arc<McpClient>) -> Result<Self, crate::McpError> {
        let snapshot = Self::build_snapshot(client.clone()).await?;
        Ok(Self {
            snapshot: RwLock::new(snapshot),
            client,
        })
    }

    /// 从 Client 拉取最新工具，更新快照。
    pub async fn update_tools(&self) -> Result<(), crate::McpError> {
        let new_snapshot = Self::build_snapshot(self.client.clone()).await?;
        // 写锁只保护 store 操作，不保护网络调用
        *self.snapshot.write().unwrap() = new_snapshot;
        Ok(())
    }

    /// 构建工具快照。
    async fn build_snapshot(client: Arc<McpClient>) -> Result<Arc<ToolSnapshot>, crate::McpError> {
        let list_result: crate::protocol::ListToolsResult = client.tools_list().await?;

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

        Ok(Arc::new(ToolSnapshot::new(reg_map, 0)))
    }

    /// 读取当前快照（短暂读锁）。
    pub fn load_full(&self) -> Arc<ToolSnapshot> {
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

    /// 获取内部 Client 引用。
    pub fn client(&self) -> &Arc<McpClient> {
        &self.client
    }
}

#[async_trait::async_trait]
impl ToolCatalog for McpCatalog {
    async fn snapshot(&self) -> Arc<ToolSnapshot> {
        self.load_full()
    }
}

/// 将 MCP 工具转换为 ExecutableTool。
///
/// `client` 是 Arc 引用，工具闭包会克隆这个 Arc 保持 Transport 存活。
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
            let call_result: crate::protocol::CallToolResult = client
                .call_tool(&name, Some(input.clone()))
                .await
                .map_err(|e| mcp_error_to_tool_error(&e))?;

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
    })
}

/// 将 McpError 映射为 ToolError。
pub(super) fn mcp_error_to_tool_error(e: &crate::McpError) -> ToolError {
    match e {
        crate::McpError::Transport(crate::protocol::TransportError::Timeout) => ToolError {
            kind: ToolErrorKind::Timeout,
            message: "mcp request timeout".to_string(),
        },
        crate::McpError::Transport(crate::protocol::TransportError::Disconnected) => ToolError {
            kind: ToolErrorKind::Network,
            message: "mcp disconnected".to_string(),
        },
        _ => ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("mcp error: {e}"),
        },
    }
}
