//! McpCatalogWatcher — 后台监听 tools/list_changed 通知，自动刷新工具目录。
//!
//! 设计要点：
//! - 依赖 `CatalogRefresh` trait 而非具体类型（Command Pattern）
//! - 不持有 Client 或 Catalog，只持有刷新能力
//! - 由 Registry 统一 spawn 和管理生命周期

use std::sync::Arc;

use lellm_mcp::protocol::JsonRpcNotification;
use tokio_util::sync::CancellationToken;

/// 工具目录刷新 trait — Watcher 通过此 trait 执行刷新，不依赖具体实现。
#[async_trait::async_trait]
pub trait CatalogRefresh: Send + Sync {
    /// 刷新工具目录。
    async fn refresh(&self) -> Result<(), lellm_mcp::McpError>;
}

/// MCP 工具目录观察者。
///
/// 后台监听 `tools/list_changed` 通知，通过 `CatalogRefresh` trait 刷新工具目录。
/// 由 Registry 统一 spawn 和管理，不自己管理生命周期。
pub struct McpCatalogWatcher {
    refresher: Arc<dyn CatalogRefresh>,
    rx: tokio::sync::broadcast::Receiver<JsonRpcNotification>,
}

impl McpCatalogWatcher {
    /// 创建观察者。
    ///
    /// `refresher` 用于刷新工具目录，`rx` 用于接收通知。
    pub fn new(
        refresher: Arc<dyn CatalogRefresh>,
        rx: tokio::sync::broadcast::Receiver<JsonRpcNotification>,
    ) -> Self {
        Self { refresher, rx }
    }

    /// 启动后台监听任务。
    ///
    /// 当收到 `tools/list_changed` 通知时，自动调用 `refresher.refresh()`。
    /// 由 Registry 统一 spawn，返回 JoinHandle 供 Registry 管理。
    ///
    /// `cancel` 用于优雅关闭——Watcher 在 `select!` 中监听取消信号。
    /// 如果任务正在执行网络 I/O（无法被 cancel 中断），
    /// 调用方应配合 `JoinHandle::abort()` 强制终止。
    pub fn spawn(mut self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    // 优先响应取消信号
                    _ = cancel.cancelled() => {
                        tracing::debug!("watcher cancelled, stopping");
                        break;
                    }
                    res = self.rx.recv() => {
                        match res {
                            Ok(notif) => {
                                if matches!(
                                    notif.kind(),
                                    lellm_mcp::protocol::NotificationKind::ToolsListChanged
                                ) {
                                    tracing::info!("tools/list_changed received, refreshing catalog");
                                    if let Err(e) = self.refresher.refresh().await {
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
                }
            }
        })
    }
}

/// 为 CatalogRefresher 实现 CatalogRefresh trait。
#[async_trait::async_trait]
impl CatalogRefresh for super::CatalogRefresher {
    async fn refresh(&self) -> Result<(), lellm_mcp::McpError> {
        self.refresh_impl().await
    }
}
