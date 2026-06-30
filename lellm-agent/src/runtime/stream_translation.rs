//! ProviderEvent → StreamChunk 转换层（Anti-Corruption Layer）。
//!
//! 职责：将 Provider 层的协议事件转换为 Graph 层的数据面事件。
//! 防止 Provider 语义泄漏到 Graph 层。
//!
//! # 设计原则
//!
//! - 信息不丢失 — 使用 `TranslationResult` 而非 `Option<StreamChunk>`
//! - 一对多映射 — 一个 ProviderEvent 可产生多个 StreamChunk
//! - Graph 不知道 Provider 的内部协议（Usage, RawChunk, FinishReason 等）
//!
//! # 演进路径
//!
//! v0.4: `translate_provider_event()` → `TranslationResult`
//! v0.5: 演进为 `ProviderEvent → AgentStreamEvent → StreamChunk` 三层模型

use lellm_graph::StreamChunk;
use lellm_provider::ProviderEvent;

/// ProviderEvent 转换结果。
///
/// 使用 enum 而非 `Option<StreamChunk>`，避免静默丢失事件信息。
/// 每个变体表达一种明确的意图：发射、发射+累积、记录 Usage、标记完成、忽略。
#[derive(Debug)]
pub enum TranslationResult {
    /// 发射一个数据面事件到 StreamSink
    Emit(StreamChunk),
    /// 发射 TextDelta 并携带增量文本（供调用方累积构建 ContentBlock）
    EmitWithText { chunk: StreamChunk, delta: String },
    /// 发射 ThinkingDelta 并携带增量思考内容（供调用方累积构建 ContentBlock）
    EmitWithThinking {
        chunk: StreamChunk,
        delta: String,
        redacted: Option<String>,
    },
    /// 记录 Usage 信息（不发射 StreamChunk，由 LLMNode 收集）
    Usage(lellm_core::TokenUsage),
    /// 响应完成标记
    Finished,
    /// 忽略此事件（如 HeadersReceived, Start 等协议事件）
    Ignore,
}

/// 将 ProviderEvent 转换为 TranslationResult。
///
/// 当前为 1:1 映射。未来可能支持 1:N（一个 ProviderEvent → 多个 StreamChunk）。
pub fn translate_provider_event(event: &ProviderEvent) -> TranslationResult {
    match event {
        ProviderEvent::Token { token } => TranslationResult::EmitWithText {
            chunk: StreamChunk::TextDelta(token.clone()),
            delta: token.clone(),
        },
        ProviderEvent::ThinkingDelta { thinking, redacted } => {
            TranslationResult::EmitWithThinking {
                chunk: StreamChunk::ThinkingDelta {
                    text: thinking.clone(),
                    redacted: redacted.clone(),
                },
                delta: thinking.clone(),
                redacted: redacted.clone(),
            }
        }
        ProviderEvent::ResponseComplete { usage, .. } => {
            if let Some(u) = usage {
                TranslationResult::Usage(u.clone())
            } else {
                TranslationResult::Finished
            }
        }
        _ => TranslationResult::Ignore,
    }
}

// ─── 测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_translation() {
        let event = ProviderEvent::Token {
            token: "hello".into(),
        };
        match translate_provider_event(&event) {
            TranslationResult::EmitWithText {
                chunk: StreamChunk::TextDelta(t),
                delta,
            } => {
                assert_eq!(t, "hello");
                assert_eq!(delta, "hello");
            }
            other => panic!("expected EmitWithText(TextDelta), got {:?}", other),
        }
    }

    #[test]
    fn test_thinking_delta_preserves_redacted() {
        let event = ProviderEvent::ThinkingDelta {
            thinking: "visible".into(),
            redacted: Some("sensitive".into()),
        };
        match translate_provider_event(&event) {
            TranslationResult::EmitWithThinking {
                chunk:
                    StreamChunk::ThinkingDelta {
                        text,
                        redacted: chunk_redacted,
                    },
                delta,
                redacted,
            } => {
                assert_eq!(text, "visible");
                assert_eq!(chunk_redacted, Some("sensitive".into()));
                assert_eq!(delta, "visible");
                assert_eq!(redacted, Some("sensitive".into()));
            }
            other => panic!("expected EmitWithThinking, got {:?}", other),
        }
    }

    #[test]
    fn test_thinking_delta_without_redacted() {
        let event = ProviderEvent::ThinkingDelta {
            thinking: "thinking".into(),
            redacted: None,
        };
        match translate_provider_event(&event) {
            TranslationResult::EmitWithThinking {
                chunk:
                    StreamChunk::ThinkingDelta {
                        text,
                        redacted: chunk_redacted,
                    },
                delta,
                redacted,
            } => {
                assert_eq!(text, "thinking");
                assert!(chunk_redacted.is_none());
                assert_eq!(delta, "thinking");
                assert!(redacted.is_none());
            }
            other => panic!("expected EmitWithThinking, got {:?}", other),
        }
    }

    #[test]
    fn test_ignore_protocol_events() {
        let event = ProviderEvent::Start {
            model: "test".into(),
        };
        assert!(matches!(
            translate_provider_event(&event),
            TranslationResult::Ignore
        ));
    }
}
