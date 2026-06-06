//! Stream processing — SSE 解析 + ToolCall 聚合 + 流式事件分发。
//!
//! 管道：Bytes → SseParser → SseFrame → Adapter → StreamChunk → EventSink
//!
//! 本模块**不知道** reqwest、tokio channel 等传输细节。
//! 只认识 `Stream<Item = Result<Bytes>>` 和 `EventSink` trait。

use lellm_core::{LlmError, TokenUsage, ToolCall};

pub(crate) mod sse_frame;
pub(crate) mod sse_parser;
pub(crate) mod stream_processor;
pub(crate) mod tool_call_accumulator;
pub(crate) mod usage_accumulator;

pub(crate) use sse_frame::SseFrame;
pub(crate) use sse_parser::SseParser;
pub(crate) use tool_call_accumulator::{ToolCallAccumulator, ToolCallDelta};
pub(crate) use usage_accumulator::{UsageAccumulator, UsageDelta};

/// 流式事件 — process_stream() 的输出单元。
///
/// 这是 stream/ 模块对外的唯一数据契约，
/// 不耦合 ProviderEvent（lib.rs 中的消费者概念）。
#[derive(Debug)]
pub(crate) enum StreamEvent {
    /// 流式开始
    Start { model: String },
    /// 文本增量（可丢弃）
    Token { token: String },
    /// 思考块增量（可丢弃）
    ThinkingDelta {
        thinking: String,
        redacted: Option<String>,
    },
    /// 解析错误（不可丢弃）
    Error(LlmError),
    /// 单次 LLM 响应结束（HTTP/SSE 请求完成，不可丢弃）
    ResponseComplete {
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
}

impl StreamEvent {
    /// 事件是否为关键状态机事件——丢失会导致消费者状态错乱。
    pub(crate) fn is_critical(&self) -> bool {
        matches!(
            self,
            StreamEvent::Error(_) | StreamEvent::ResponseComplete { .. }
        )
    }
}

/// 事件输出接口 — process_stream() 的唯一输出通道。
///
/// 解耦 stream/ 模块与具体传输机制（tokio channel, callback, mock 等）。
/// 测试时只需实现此 trait 即可构造 mock sink。
///
/// **async** — 关键事件（Error, ResponseComplete）需要阻塞等待送达。
/// **emit 返回 bool** — `false` 表示 channel 已关闭，调用方应立即退出。
/// **is_closed** — 零开销探测，用于在耗时的解析工作前快速退出。
pub trait EventSink {
    /// 发送事件。返回 `false` 表示消费者已断开，应立即退出。
    async fn emit(&mut self, event: StreamEvent) -> bool;

    /// 消费者是否已断开。用于在解析开销前快速退出。
    /// 默认返回 `false`（测试 mock 无需覆盖）。
    fn is_closed(&self) -> bool {
        false
    }
}
