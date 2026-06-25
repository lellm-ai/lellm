//! StreamChunk — Data Plane 数据面事件。
//!
//! 高频、数据透传、无嵌套包装。
//! 与控制面 RuntimeEvent 分离，避免高频数据事件撑爆控制事件通道。
//!
//! 设计原则：只承载"需要实时展示给用户的内容"，不混入状态变更等低频数据。
//! StreamChunk 携带 Execution View（展示内容），不是 Message。
//! State 保存完整 Message。两者永不互相引用。

use std::time::Duration;

// ─── ToolPhase ────────────────────────────────────────────────

/// 工具执行生命周期阶段。
///
/// 与 LangGraph (`on_tool_start`/`on_tool_end`)、
/// OpenAI Agents SDK (`tool_call_started`/`tool_call_finished`)、
/// Claude Desktop (`tool_use`/`tool_result`) 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    /// 工具已加入执行队列（尚未开始）
    Queued,
    /// 工具开始执行
    Started,
    /// 工具执行完成（成功或失败）
    Finished,
}

// ─── StreamChunk ──────────────────────────────────────────────

/// 数据面事件 — 高频、数据透传。
///
/// 统一流式协议，所有 Node（LLM、Tool、MCP、Workflow）共享。
///
/// **Tool 并发 emit 协议：**
/// - Start 保证顺序 — 严格按照 ToolCall 顺序发射（A, B, C）
/// - End 允许乱序 — 并发执行完成后按实际顺序发射（B, A, C），通过 call_id 关联
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// 文本输出（LLM 生成的文本 token）
    TextDelta(String),
    /// 思考内容（LLM 的 reasoning/thinking block）
    ThinkingDelta(String),
    /// 工具生命周期事件（Queued / Started / Finished）
    ToolLifecycle {
        phase: ToolPhase,
        call_id: String,
        tool_name: String,
    },
    /// 工具执行结果（展示用，content 为 String，前端直接展示）
    ///
    /// 与 State Plane 的 `Message::ToolResult` 分离。
    /// State 保存完整 Message（含 content_blocks, metadata, raw_response）。
    /// 此变体仅用于实时展示。
    ToolOutput {
        call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
        duration: Duration,
    },
}
