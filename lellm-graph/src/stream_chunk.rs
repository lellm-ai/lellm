//! StreamChunk — Data Plane 数据面事件。
//!
//! 高频、数据透传、无嵌套包装。
//! 与控制面 RuntimeEvent 分离，避免高频数据事件撑爆控制事件通道。

// ─── StreamChunk ──────────────────────────────────────────────

/// 数据面事件 — 高频、数据透传。
///
/// 设计原则：只承载"需要实时展示给用户的内容"，不混入状态变更等低频数据。
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 文本输出（LLM 生成的文本 token）
    Text(String),
    /// 思考内容（LLM 的 reasoning/thinking block）
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
}
