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
use crate::providers::codec::{ChatCodec, StreamChunk};

/// 单个 SseFrame 的解析结果。
struct FrameResult {
    text: Option<String>,
    thinking: Option<String>,
    thinking_redacted: Option<String>,
    /// 一帧可能含多个工具调用增量（并行工具调用），用 Vec 保留全部，
    /// 避免单 Option 互相覆盖导致并行调用丢失。
    tool_call_deltas: Vec<ToolCallDelta>,
    usage_delta: Option<UsageDelta>,
    is_done: bool,
}

/// 处理 SSE 字节流，将 StreamEvent 发送到 sink。
///
/// 管道：Bytes → SseParser → SseFrame → Codec → StreamChunk → StreamEvent
///
/// **永远转发所有协议事件**（含 ThinkingDelta）。
/// 是否向消费者展示 ThinkingDelta 由 Agent 层 `stream_thinking` 控制。
///
/// # 参数
/// - `sink`: 事件输出端
/// - `codec`: ChatCodec，负责 SSE data → StreamChunk 的协议解析
/// - `model`: 模型标识
/// - `bytes_stream`: 任意字节流（reqwest、hyper、mock、file...）
///
/// # 泛型参数
/// - `S`: 任意字节流
/// - `A`: ChatCodec 实现
/// - `E`: 事件输出端
pub async fn process_stream<S, A, E>(sink: &mut E, codec: &A, model: String, mut bytes_stream: S)
where
    S: Stream<Item = Result<Bytes, LlmError>> + Unpin,
    A: ChatCodec,
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
                    let fr = match handle_frame(codec, &frame) {
                        Ok(fr) => fr,
                        Err(e) => {
                            // 关键数据损坏 — 帧本应携带数据却无法可靠解析。
                            // 发 Error 并中止（对齐下方字节流 Err 路径）；不再静默吞掉。
                            // 日志只记录错误与长度，不输出原始帧或工具参数。
                            tracing::error!(
                                elapsed = ?stream_start.elapsed(),
                                error = %e,
                                data_len = frame.data.len(),
                                "critical frame decode error — aborting stream"
                            );
                            sink.emit(StreamEvent::Error(e)).await;
                            return;
                        }
                    };

                    // 文本增量
                    if let Some(text) = fr.text
                        && !sink.emit(StreamEvent::Token { token: text }).await
                    {
                        return;
                    }

                    // 思考增量 — 永远转发（协议事件）。
                    // 是否向消费者展示由 Agent 层 stream_thinking 控制。
                    if let Some(thinking) = fr.thinking
                        && !sink
                            .emit(StreamEvent::ThinkingDelta {
                                thinking,
                                redacted: fr.thinking_redacted,
                            })
                            .await
                    {
                        return;
                    }

                    // ToolCall 增量 — 一帧可能含多个（并行工具调用），全部入累积器
                    for delta in &fr.tool_call_deltas {
                        tool_call_acc.push(delta);
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

/// 处理单个 SseFrame — 调用 Codec 解析，返回结构化结果。
///
/// 帧错误分类（复用 `decode_sse` 的 `Ok`/`Err` 契约）：
/// - `Ok` — 帧已安全解析（含良性 no-op：空帧 / 结束信号 / 未知事件 / 有效 JSON 但无相关字段）
///   → 提取 chunks，调用方继续处理后续帧。
/// - `Err` — 无法安全继续处理的解码错误（关键数据损坏）→ 此处用 `?` 传播，
///   由 `process_stream` 发 `StreamEvent::Error` 并中止。绝不静默吞掉。
fn handle_frame<A: ChatCodec>(codec: &A, frame: &SseFrame) -> Result<FrameResult, LlmError> {
    let mut result = FrameResult {
        text: None,
        thinking: None,
        thinking_redacted: None,
        tool_call_deltas: Vec::new(),
        usage_delta: None,
        is_done: false,
    };

    let parse_result = codec.decode_sse(frame)?;

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
                result.tool_call_deltas.push(ToolCallDelta {
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

    Ok(result)
}

// NOTE: process_stream 的单元测试需要 Mock Adapter。
// 由于 ProviderCodec trait 涉及 parse_sse_frame(JSON 解析)，
// 完整的集成测试放在 tests/integration.rs 中。
// SseParser, ToolCallAccumulator, UsageAccumulator 已有独立的单元测试。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::google::GoogleCodec;

    /// 回归：一帧含多个并行工具调用增量时，FrameResult 必须保留全部。
    /// 此前 `tool_call_delta` 为单 `Option`，同帧第二个 delta 覆盖第一个 → 并行调用丢失。
    #[test]
    fn test_handle_frame_preserves_parallel_tool_call_deltas() {
        let frame = SseFrame {
            event: None,
            data: serde_json::json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            { "functionCall": { "name": "get_weather", "args": { "city": "SF" } } },
                            { "functionCall": { "name": "get_time", "args": {} } }
                        ],
                        "role": "model"
                    },
                    "finishReason": "STOP"
                }]
            })
            .to_string(),
        };

        let fr = handle_frame(&GoogleCodec, &frame).unwrap();
        assert_eq!(fr.tool_call_deltas.len(), 2);
        assert_eq!(fr.tool_call_deltas[0].index, 0);
        assert_eq!(fr.tool_call_deltas[1].index, 1);
        assert_eq!(fr.tool_call_deltas[0].name.as_deref(), Some("get_weather"));
        assert_eq!(fr.tool_call_deltas[1].name.as_deref(), Some("get_time"));
    }

    /// 分类单测：损坏帧（非法 JSON）→ handle_frame 返回 Err（关键数据损坏）。
    #[test]
    fn test_handle_frame_corrupt_returns_err() {
        let frame = SseFrame {
            event: None,
            data: "{not valid json".into(),
        };
        assert!(handle_frame(&GoogleCodec, &frame).is_err());
    }

    /// 分类单测：可忽略帧（合法 JSON 但无相关字段）→ 返回 Ok 且无 chunks。
    #[test]
    fn test_handle_frame_ignorable_returns_ok_empty() {
        let frame = SseFrame {
            event: None,
            data: serde_json::json!({"foo": "bar"}).to_string(),
        };
        let fr = handle_frame(&GoogleCodec, &frame).unwrap();
        assert!(fr.text.is_none());
        assert!(fr.tool_call_deltas.is_empty());
        assert!(fr.usage_delta.is_none());
        assert!(!fr.is_done);
    }

    // ─── 帧错误分类：process_stream 行为测试 ────────────────────
    //
    // 用真实 GoogleCodec + 记录型 sink 覆盖验收点：
    // 损坏帧 → 发 Error 并中止（不再静默吞掉）；可忽略帧 → 继续；
    // 结束信号 → 按 codec 语义完成（非 no-op）；EOF 收尾损坏 → 同样传播。

    use futures_util::stream;

    /// 记录型 EventSink — 捕获 process_stream 发出的全部事件。
    #[derive(Default)]
    struct RecordingSink {
        events: Vec<StreamEvent>,
    }

    impl EventSink for RecordingSink {
        async fn emit(&mut self, event: StreamEvent) -> bool {
            self.events.push(event);
            true
        }
    }

    /// 包装为 SSE 帧（`data: <payload>\n\n`，尾随空行是帧边界）。
    fn sse_frame(payload: &str) -> String {
        format!("data: {payload}\n\n")
    }

    /// 构造字节流：每个元素是一个完整 SSE 帧。
    fn frames_stream(frames: Vec<String>) -> impl Stream<Item = Result<Bytes, LlmError>> + Unpin {
        stream::iter(
            frames
                .into_iter()
                .map(|f| Ok::<_, LlmError>(Bytes::from(f))),
        )
    }

    /// 提取事件类型名，便于断言。
    fn kinds(events: &[StreamEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                StreamEvent::Start { .. } => "Start",
                StreamEvent::Token { .. } => "Token",
                StreamEvent::ThinkingDelta { .. } => "ThinkingDelta",
                StreamEvent::Error(_) => "Error",
                StreamEvent::ResponseComplete { .. } => "ResponseComplete",
            })
            .collect()
    }

    /// 提取所有 Token 事件的文本。
    fn tokens(events: &[StreamEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Token { token } => Some(token.clone()),
                _ => None,
            })
            .collect()
    }

    /// 正常帧 → 损坏帧 → 正常帧：前面的正常事件保留；只发一次 Error；后续帧不再处理。
    #[tokio::test]
    async fn test_corrupt_frame_aborts_and_skips_rest() {
        let frames = vec![
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"}}]})
                    .to_string(),
            ),
            sse_frame("{this is not valid json"),
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"text":"bye"}],"role":"model"}}]})
                    .to_string(),
            ),
        ];
        let mut sink = RecordingSink::default();
        process_stream(&mut sink, &GoogleCodec, "m".into(), frames_stream(frames)).await;

        let k = kinds(&sink.events);
        assert_eq!(k, vec!["Start", "Token", "Error"]);
        assert_eq!(k.iter().filter(|e| **e == "Error").count(), 1);
        assert!(!tokens(&sink.events).contains(&"bye".to_string()));
        assert!(!k.contains(&"ResponseComplete"));
    }

    /// 损坏发生时已有部分工具参数：不输出拼接中的工具调用，不发 ResponseComplete。
    #[tokio::test]
    async fn test_corrupt_with_partial_tool_args_no_response_complete() {
        let frames = vec![
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"search","args":{"q":"par"}}}],"role":"model"}}]})
                    .to_string(),
            ),
            sse_frame("{broken"),
        ];
        let mut sink = RecordingSink::default();
        process_stream(&mut sink, &GoogleCodec, "m".into(), frames_stream(frames)).await;

        let k = kinds(&sink.events);
        assert_eq!(k, vec!["Start", "Error"]);
        assert!(!k.contains(&"ResponseComplete"));
    }

    /// 空帧／明确可忽略事件 → 正常帧：继续处理正常数据。
    /// 注：当前 codec 对「合法但无相关字段」的帧返回 Ok(empty) 而忽略——
    /// 这里只验证流不中断，不宣称该帧一定是良性事件。
    #[tokio::test]
    async fn test_continues_past_ignored_frame() {
        let frames = vec![
            sse_frame(&serde_json::json!({"foo":"bar"}).to_string()),
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"}}]})
                    .to_string(),
            ),
        ];
        let mut sink = RecordingSink::default();
        process_stream(&mut sink, &GoogleCodec, "m".into(), frames_stream(frames)).await;

        let k = kinds(&sink.events);
        assert_eq!(k, vec!["Start", "Token", "ResponseComplete"]);
        assert_eq!(tokens(&sink.events), vec!["hi".to_string()]);
    }

    /// 正常结束信号（finishReason）：按 codec 语义完成，不归入 no-op（后续帧不处理）。
    #[tokio::test]
    async fn test_end_signal_completes_not_noop() {
        let frames = vec![
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"},"finishReason":"STOP"}]})
                    .to_string(),
            ),
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"text":"AFTER_DONE"}],"role":"model"}}]})
                    .to_string(),
            ),
        ];
        let mut sink = RecordingSink::default();
        process_stream(&mut sink, &GoogleCodec, "m".into(), frames_stream(frames)).await;

        let k = kinds(&sink.events);
        assert_eq!(k, vec!["Start", "Token", "ResponseComplete"]);
        assert_eq!(tokens(&sink.events), vec!["hi".to_string()]);
        assert!(!tokens(&sink.events).contains(&"AFTER_DONE".to_string()));
    }

    /// 最后一帧在 EOF 收尾时解析失败：同样传播 Error，不能吞掉。
    #[tokio::test]
    async fn test_last_frame_corrupt_at_eof_propagates_error() {
        let frames = vec![
            sse_frame(
                &serde_json::json!({"candidates":[{"content":{"parts":[{"text":"hi"}],"role":"model"}}]})
                    .to_string(),
            ),
            sse_frame("{corrupt tail"),
        ];
        let mut sink = RecordingSink::default();
        process_stream(&mut sink, &GoogleCodec, "m".into(), frames_stream(frames)).await;

        let k = kinds(&sink.events);
        assert_eq!(k, vec!["Start", "Token", "Error"]);
        assert!(!k.contains(&"ResponseComplete"));
    }
}
