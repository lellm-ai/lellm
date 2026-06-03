//! Stream Processor — SSE 字节流 → ProviderEvent 分发的完整管道。
//!
//! 职责：orchestrate SseParser + Adapter + ToolCallAccumulator，
//! 将结果通过 channel 发送给消费者。

use futures_util::StreamExt;
use lellm_core::{LlmError, TokenUsage};
use tokio::sync::mpsc::Sender;

use super::sse_frame::SseFrame;
use super::sse_parser::SseParser;
use super::tool_call_accumulator::{ToolCallAccumulator, ToolCallDelta};
use crate::ProviderEvent;
use crate::providers::base::{ProviderAdapter, StreamChunk};

/// 处理 SSE 字节流，将 ProviderEvent 发送到 channel。
///
/// 管道：bytes → SseParser → SseFrame → Adapter → StreamChunk → ProviderEvent
pub async fn process_stream<A: ProviderAdapter>(
    tx: Sender<Result<ProviderEvent, LlmError>>,
    model: String,
    adapter: A,
    bytes_stream: reqwest::Response,
) {
    let _ = tx.send(Ok(ProviderEvent::Start { model })).await;

    let mut parser = SseParser::new();
    let mut accumulator = ToolCallAccumulator::new();
    let mut usage: Option<TokenUsage> = None;
    let mut is_done = false;

    let mut boxed_stream = Box::pin(bytes_stream.bytes_stream());

    while let Some(result) = boxed_stream.next().await {
        match result {
            Ok(bytes) => {
                let frames = parser.feed(&bytes);

                for frame in frames {
                    let (done, u) = handle_frame(&tx, &adapter, &mut accumulator, &frame).await;
                    if done {
                        is_done = true;
                    }
                    if let Some(u) = u {
                        usage = Some(u);
                    }
                }
            }
            Err(e) => {
                let _ = tx
                    .send(Err(LlmError::Network {
                        detail: e.to_string(),
                    }))
                    .await;
                return;
            }
        }

        if is_done {
            break;
        }
    }

    let tool_calls = accumulator.finalize().unwrap_or_default();
    let _ = tx.send(Ok(ProviderEvent::Done { tool_calls, usage })).await;
}

/// 处理单个 SseFrame — 调用 Adapter 解析，分发 StreamChunk。
async fn handle_frame<A: ProviderAdapter>(
    tx: &Sender<Result<ProviderEvent, LlmError>>,
    adapter: &A,
    accumulator: &mut ToolCallAccumulator,
    frame: &SseFrame,
) -> (bool, Option<TokenUsage>) {
    let mut is_done = false;
    let mut usage = None;

    match adapter.parse_sse_frame(frame) {
        Ok(result) => {
            for c in result.chunks {
                match c {
                    StreamChunk::TextDelta(text) => {
                        let _ = tx.send(Ok(ProviderEvent::Token { token: text })).await;
                    }
                    StreamChunk::ToolCallDelta(delta) => {
                        accumulator.push(&ToolCallDelta {
                            index: delta.index,
                            id: delta.id,
                            name: delta.name,
                            arguments_delta: delta.arguments_delta,
                        });
                    }
                    StreamChunk::Usage(u) => {
                        usage = Some(u);
                    }
                    StreamChunk::Done => {
                        is_done = true;
                    }
                }
            }
        }
        Err(e) => {
            let _ = tx.send(Err(e)).await;
        }
    }

    (is_done, usage)
}
