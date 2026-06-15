//! JSON-RPC Notification。

use serde::{Deserialize, Serialize};

/// JSON-RPC 2.0 Notification（无 id 的 Request）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    #[serde(rename = "method")]
    pub method_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// MCP Notification 方法名称。
pub mod methods {
    pub const INITIALIZED: &str = "notifications/initialized";
    pub const TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
    pub const PROGRESS: &str = "notifications/progress";
}

/// 通知类型枚举。
#[derive(Debug, Clone)]
pub enum NotificationKind {
    /// Server 已收到 initialize 确认。
    Initialized,
    /// 工具列表已变更。
    ToolsListChanged,
    /// 进度通知。
    Progress { progress: u64, total: Option<u64> },
    /// 其他通知。
    Other {
        method: String,
        params: Option<serde_json::Value>,
    },
}

impl JsonRpcNotification {
    /// 解析为 NotificationKind。
    pub fn kind(&self) -> NotificationKind {
        match self.method_name.as_str() {
            methods::INITIALIZED => NotificationKind::Initialized,
            methods::TOOLS_LIST_CHANGED => NotificationKind::ToolsListChanged,
            methods::PROGRESS => {
                let (progress, total) = if let Some(params) = &self.params {
                    let progress = params.get("progress").and_then(|v| v.as_u64()).unwrap_or(0);
                    let total = params.get("total").and_then(|v| v.as_u64());
                    (progress, total)
                } else {
                    (0, None)
                };
                NotificationKind::Progress { progress, total }
            }
            _ => NotificationKind::Other {
                method: self.method_name.clone(),
                params: self.params.clone(),
            },
        }
    }
}
