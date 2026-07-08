//! McpCatalogWatcher — 后台监听 tools/list_changed 通知，自动刷新工具目录。

use super::catalog::McpCatalog;
use crate::client::McpClient;
use crate::protocol::JsonRpcNotification;

/// MCP 工具目录观察者。
///
/// 后台监听 `tools/list_changed` 通知，自动刷新 McpCatalog。
/// 创建后调用 `.spawn()` 启动后台任务。
pub struct McpCatalogWatcher {
    catalog: std::sync::Arc<McpCatalog>,
    rx: tokio::sync::broadcast::Receiver<JsonRpcNotification>,
}

impl McpCatalogWatcher {
    /// 创建观察者。
    ///
    /// `client` 用于订阅 notifications，`catalog` 用于更新快照。
    pub fn new(catalog: std::sync::Arc<McpCatalog>, client: &std::sync::Arc<McpClient>) -> Self {
        let rx = client.subscribe_notifications().unwrap_or_else(|| {
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            drop(tx);
            rx
        });
        Self { catalog, rx }
    }

    /// 启动后台监听任务。
    ///
    /// 当收到 `tools/list_changed` 通知时，自动调用 `catalog.update_tools()`。
    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.rx.recv().await {
                    Ok(notif) => {
                        if matches!(
                            notif.kind(),
                            crate::protocol::NotificationKind::ToolsListChanged
                        ) {
                            tracing::info!("tools/list_changed received, refreshing catalog");
                            if let Err(e) = self.catalog.update_tools().await {
                                tracing::warn!(error = %e, "failed to refresh catalog");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(skipped = n, "notification lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::debug!("notification channel closed, watcher stopping");
                        break;
                    }
                }
            }
        })
    }
}
