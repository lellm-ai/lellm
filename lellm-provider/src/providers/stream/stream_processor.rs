//! Stream Processor — SSE 字节流 → StreamEvent 分发的纯协议管道。
//!
//! 职责：orchestrate SseParser + Adapter + ToolCallAccumulator + UsageAccumulator，
//! 将结果通过 EventSink 输出。
//!
//! **不知道** reqwest、tokio channel 等传输细节。
//! 只认识 `Stream<Item = Result<Bytes, LlmError>>` 和 `EventSink` trait。

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use lellm_core::LlmError;

use super::{
    EventSink, SseFrame, SseParser, StreamEvent, ToolCallAccumulator, ToolCallDelta,
    UsageAccumulator, UsageDelta,
};
use crate::providers::base::{ProviderAdapter, StreamChunk};

/// 单个 SseFrame 的解析结果。
struct FrameResult {
    text: Option<String>,
    thinking: Option<String>,
    thinking_redacted: Option<String>,
    tool_call_delta: Option<ToolCallDelta>,
    usage_delta: Option<UsageDelta>,
    is_done: bool,
}

/// 处理 SSE 字节流，将 StreamEvent 发送到 sink。
///
/// 管道：Bytes → SseParser → SseFrame → Adapter → StreamChunk → StreamEvent
///
/// # 参数
/// - `sink`: 事件输出端
/// - `adapter`: Provider 适配器，负责 SSE data → StreamChunk 的协议解析
/// - `model`: 模型标识
/// - `stream_thinking`: 是否向消费者发射 ThinkingDelta 事件
/// - `bytes_stream`: 任意字节流（reqwest、hyper、mock、file...）
///
/// # 泛型参数
/// - `S`: 任意字节流
/// - `A`: Provider 适配器
/// - `E`: 事件输出端
pub async fn process_stream<S, A, E>(
    sink: &mut E,
    adapter: &A,
    model: String,
    stream_thinking: bool,
    mut bytes_stream: S,
) where
    S: Stream<Item = Result<Bytes, LlmError>> + Unpin,
    A: ProviderAdapter,
    E: EventSink,
{
    // Start 事件 — 消费者尚未连接则立即退出
    if !sink.emit(StreamEvent::Start { model }).await {
        return;
    }

    let mut parser = SseParser::new();
    let mut tool_call_acc = ToolCallAccumulator::new();
    let mut usage_acc = UsageAccumulator::new();
    let mut is_done = false;

    let stream_start = std::time::Instant::now();
    while let Some(result) = bytes_stream.next().await {
        // 在解析开销前快速探测 channel 是否断开
        if sink.is_closed() {
            return;
        }

        match result {
            Ok(bytes) => {
                let frames = parser.feed(&bytes);

                for frame in frames {
                    let fr = handle_frame(adapter, &frame);

                    // 文本增量
                    if let Some(text) = fr.text {
                        if !sink.emit(StreamEvent::Token { token: text }).await {
                            return;
                        }
                    }

                    // 思考增量 — 根据 stream_thinking 决定是否发射
                    if stream_thinking {
                        if let Some(thinking) = fr.thinking {
                            if !sink
                                .emit(StreamEvent::ThinkingDelta {
                                    thinking,
                                    redacted: fr.thinking_redacted,
                                })
                                .await
                            {
                                return;
                            }
                        }
                    }

                    // ToolCall 增量
                    if let Some(delta) = fr.tool_call_delta {
                        tool_call_acc.push(&delta);
                    }

                    // Usage 增量
                    if let Some(delta) = fr.usage_delta {
                        usage_acc.push(&delta);
                    }

                    // 结束标记
                    if fr.is_done {
                        is_done = true;
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    elapsed = ?stream_start.elapsed(),
                    error = %e,
                    "stream error"
                );
                sink.emit(StreamEvent::Error(e)).await;
                return;
            }
        }

        if is_done {
            break;
        }
    }

    // 消费者已断开 — 跳过 ResponseComplete 的发送开销
    if sink.is_closed() {
        return;
    }

    let tool_calls = tool_call_acc.finalize().unwrap_or_default();
    let final_usage = usage_acc.finalize();
    sink.emit(StreamEvent::ResponseComplete {
        tool_calls,
        usage: final_usage,
    })
    .await;
}

/// 处理单个 SseFrame — 调用 Adapter 解析，返回结构化结果。
fn handle_frame<A: ProviderAdapter>(adapter: &A, frame: &SseFrame) -> FrameResult {
    let mut result = FrameResult {
        text: None,
        thinking: None,
        thinking_redacted: None,
        tool_call_delta: None,
        usage_delta: None,
        is_done: false,
    };

    match adapter.parse_sse_frame(frame) {
        Ok(parse_result) => {
            for chunk in parse_result.chunks {
                match chunk {
                    StreamChunk::TextDelta(text) => {
                        result.text = Some(text);
                    }
                    StreamChunk::ThinkingDelta { thinking, redacted } => {
                        result.thinking = Some(thinking);
                        result.thinking_redacted = redacted;
                    }
                    StreamChunk::ToolCallDelta(delta) => {
                        result.tool_call_delta = Some(ToolCallDelta {
                            index: delta.index,
                            id: delta.id.clone(),
                            name: delta.name.clone(),
                            arguments_delta: delta.arguments_delta.clone(),
                        });
                    }
                    StreamChunk::Usage(u) => {
                        result.usage_delta = Some(UsageDelta::Full(u));
                    }
                    StreamChunk::InputTokens(it) => {
                        result.usage_delta = Some(UsageDelta::InputTokens(it));
                    }
                    StreamChunk::OutputTokens(ot) => {
                        result.usage_delta = Some(UsageDelta::OutputTokens(ot));
                    }
                    StreamChunk::Done => {
                        result.is_done = true;
                    }
                }
            }
        }
        Err(e) => {
            // [DONE]、空 frame 等是可预期的跳过，不报警
            if frame.data != "[DONE]" && !frame.data.is_empty() {
                tracing::warn!(
                    error = %e,
                    data_len = frame.data.len(),
                    "failed to parse provider SSE frame"
                );
            }
        }
    }

    result
}

// NOTE: process_stream 的单元测试需要 Mock Adapter。
// 由于 ProviderAdapter trait 涉及 parse_sse_frame(JSON 解析)，
// 完整的集成测试放在 tests/integration.rs 中。
// SseParser, ToolCallAccumulator, UsageAccumulator 已有独立的单元测试。
