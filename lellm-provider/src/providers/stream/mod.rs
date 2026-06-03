//! Stream processing — SSE 解析 + ToolCall 聚合 + 流式事件分发。

pub(crate) mod sse_frame;
pub(crate) mod sse_parser;
pub(crate) mod stream_processor;
pub(crate) mod tool_call_accumulator;
