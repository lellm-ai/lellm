//! StreamChunk — Data Plane 数据透传事件。
//!
//! 高频、数据透传、无嵌套包装。
//! Token 数据面事件（高频）与控制面事件（低频）共用同一通道。

use crate::delta::StateDelta;

// ─── StreamChunk ──────────────────────────────────────────────

/// 数据面事件 — 高频、数据透传。
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 文本输出
    Text(String),
    /// 思考内容
    Thinking(String),
    /// 工具调用开始
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// 工具执行结果
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    /// 状态变更
    StateChanged(StateDelta),
}
