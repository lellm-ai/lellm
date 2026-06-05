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
    /// 文本增量
    Token { token: String },
    /// 解析错误
    Error(LlmError),
    /// 单次 LLM 响应结束（HTTP/SSE 请求完成）
    ResponseComplete {
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
}

/// 事件输出接口 — process_stream() 的唯一输出通道。
///
/// 解耦 stream/ 模块与具体传输机制（tokio channel, callback, mock 等）。
/// 测试时只需实现此 trait 即可构造 mock sink。
pub trait EventSink {
    fn emit(&mut self, event: StreamEvent);
}
