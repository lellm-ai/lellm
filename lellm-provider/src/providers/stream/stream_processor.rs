//! Stream Processor — SSE 字节流 → StreamEvent 分发的纯协议管道。
//!
//! 职责：orchestrate SseParser + Adapter + ToolCallAccumulator，
//! 将结果通过 EventSink 输出。
//!
//! **不知道** reqwest、tokio channel 等传输细节。
//! 只认识 `Stream<Item = Result<Bytes, LlmError>>` 和 `EventSink` trait。

use bytes::Bytes;
use futures_core::Stream;
use futures_util::StreamExt;
use lellm_core::{LlmError, TokenUsage};

use super::{EventSink, SseFrame, SseParser, StreamEvent, ToolCallAccumulator, ToolCallDelta};
use crate::providers::base::{ProviderAdapter, StreamChunk};

/// 单个 SseFrame 的解析结果。
struct FrameResult {
    text: Option<String>,
    tool_call_delta: Option<ToolCallDelta>,
    usage: Option<TokenUsage>,
    input_tokens: Option<u32>,
    is_done: bool,
}

/// 处理 SSE 字节流，将 StreamEvent 发送到 sink。
///
/// 管道：Bytes → SseParser → SseFrame → Adapter → StreamChunk → StreamEvent
///
/// # 泛型参数
/// - `S`: 任意字节流（reqwest、hyper、mock、file...）
/// - `A`: Provider 适配器，负责 SSE data → StreamChunk 的协议解析
/// - `E`: 事件输出端，负责将 StreamEvent 转发给消费者
pub async fn process_stream<S, A, E>(sink: &mut E, adapter: &A, model: String, mut bytes_stream: S)
where
    S: Stream<Item = Result<Bytes, LlmError>> + Unpin,
    A: ProviderAdapter,
    E: EventSink,
{
    sink.emit(StreamEvent::Start { model });

    let mut parser = SseParser::new();
    let mut accumulator = ToolCallAccumulator::new();
    let mut usage: Option<TokenUsage> = None;
    let mut input_tokens: u32 = 0;
    let mut is_done = false;

    while let Some(result) = bytes_stream.next().await {
        match result {
            Ok(bytes) => {
                let frames = parser.feed(&bytes);

                for frame in frames {
                    let fr = handle_frame(adapter, &frame);

                    // 文本增量
                    if let Some(text) = fr.text {
                        sink.emit(StreamEvent::Token { token: text });
                    }

                    // ToolCall 增量
                    if let Some(delta) = fr.tool_call_delta {
                        accumulator.push(&delta);
                    }

                    // Usage
                    if let Some(u) = fr.usage {
                        usage = Some(u);
                    }

                    // Input tokens (Anthropic message_start 事件)
                    if let Some(its) = fr.input_tokens {
                        input_tokens = its;
                    }

                    // 结束标记
                    if fr.is_done {
                        is_done = true;
                    }
                }
            }
            Err(e) => {
                sink.emit(StreamEvent::Error(e));
                return;
            }
        }

        if is_done {
            break;
        }
    }

    let tool_calls = accumulator.finalize().unwrap_or_default();
    // 合并 input_tokens（Anthropic message_start 事件中携带）
    let final_usage = match usage {
    Some(mut u) => {
        if input_tokens > 0 {
            u.prompt_tokens = input_tokens;
        }
        // 修正 total_tokens（流式 usage 可能未携带 total）
        if u.total_tokens == 0 {
            u.total_tokens = u.prompt_tokens + u.completion_tokens;
        }
        Some(u)
    }
    None if input_tokens > 0 => Some(TokenUsage {
        prompt_tokens: input_tokens,
        completion_tokens: 0,
        total_tokens: input_tokens,
    }),
    None => None,
};
    sink.emit(StreamEvent::ResponseComplete {
        tool_calls,
        usage: final_usage,
    });
}

/// 处理单个 SseFrame — 调用 Adapter 解析，返回结构化结果。
fn handle_frame<A: ProviderAdapter>(adapter: &A, frame: &SseFrame) -> FrameResult {
    let mut result = FrameResult {
        text: None,
        tool_call_delta: None,
        usage: None,
        input_tokens: None,
        is_done: false,
    };

    match adapter.parse_sse_frame(frame) {
        Ok(parse_result) => {
            for chunk in parse_result.chunks {
                match chunk {
                    StreamChunk::TextDelta(text) => {
                        result.text = Some(text);
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
                        result.usage = Some(u);
                    }
                    StreamChunk::InputTokens(it) => {
                        result.input_tokens = Some(it);
                    }
                    StreamChunk::Done => {
                        result.is_done = true;
                    }
                }
            }
        }
        Err(_) => {
            // 单帧解析失败，跳过继续处理后续帧
        }
    }

    result
}

// NOTE: process_stream 的单元测试需要 Mock Adapter。
// 由于 ProviderAdapter trait 涉及 parse_sse_frame(JSON 解析)，
// 完整的集成测试放在 tests/integration.rs 中。
// SseParser 和 ToolCallAccumulator 已有独立的单元测试。
