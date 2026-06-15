//! SSE Frame — SseParser 的输出，Adapter 的输入。

/// SSE 帧 — 由 SseParser 从字节流中构建，Adapter 只解析 payload。
#[derive(Debug, Clone)]
pub struct SseFrame {
    /// event 字段（可选），如 "message_start", "content_block_delta"
    #[allow(dead_code)]
    pub event: Option<String>,
    /// data 字段内容（通常是 JSON 字符串或标记如 "[DONE]"）
    pub data: String,
}
