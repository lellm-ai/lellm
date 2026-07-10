//! ProviderEvent → AgentStreamEvent → StreamChunk 三层转换模型。
//!
//! 职责：将 Provider 层的协议事件转换为 Graph 层的数据面事件。
//! 防止 Provider 语义泄漏到 Graph 层。
//!
//! # 架构
//!
//! ```text
//! ProviderEvent      ← Provider 层协议事件（Token, ThinkingDelta, ResponseComplete...）
//!      ↓ transform()
//! AgentStreamEvent   ← Agent 层语义事件（TextDelta, ThinkingDelta, Usage, ResponseComplete...）
//!      ↓ to_chunk()
//! StreamChunk        ← Graph 层数据面事件（TextDelta, ThinkingDelta, ToolLifecycle...）
//! ```
//!
//! # 设计原则
//!
//! - 信息不丢失 — 使用 `TranslationResult` 而非 `Option<StreamChunk>`
//! - 一对多映射 — 一个 ProviderEvent 可产生多个 AgentStreamEvent
//! - Graph 不知道 Provider 的内部协议（Usage, RawChunk, FinishReason 等）
//! - AgentStreamEvent 是封闭、强类型、exhaustive match
//!
//! # 演进路径
//!
//! v0.4: `translate_provider_event()` → `TranslationResult`
//! v0.5: `ProviderEvent → AgentStreamEvent → StreamChunk` 三层模型

use lellm_core::TokenUsage;
use lellm_graph::StreamChunk;
use lellm_provider::ProviderEvent;

// ─── AgentStreamEvent ──────────────────────────────────────────

/// Agent 层流式事件 — 语义中间层。
///
/// 桥接 Provider 协议事件与 Graph 数据面事件。
/// 封闭、强类型、exhaustive match。
#[derive(Debug, Clone)]
pub enum AgentStreamEvent {
    /// 文本增量
    TextDelta { text: String },
    /// 思考增量（thinking + 可选的 redacted）
    ThinkingDelta {
        thinking: String,
        redacted: Option<String>,
    },
    /// 工具调用结果（由 ToolNode 产生，不来自 Provider）
    ToolResult {
        call_id: String,
        tool_name: String,
        content: String,
        is_error: bool,
    },
    /// Token 使用统计
    Usage(TokenUsage),
    /// 单次 LLM 响应完成
    ResponseComplete,
}

impl AgentStreamEvent {
    /// 转换为 Graph 数据面事件。
    ///
    /// 并非所有 AgentStreamEvent 都有对应的 StreamChunk：
    /// - TextDelta → TextDelta
    /// - ThinkingDelta → ThinkingDelta
    /// - Usage / ResponseComplete → None（控制面信息，不发射到数据面）
    /// - ToolResult → None（由 ToolNode 转换为 ToolLifecycle / ToolOutput）
    pub fn to_chunk(&self) -> Option<StreamChunk> {
        match self {
            AgentStreamEvent::TextDelta { text } => Some(StreamChunk::TextDelta(text.clone())),
            AgentStreamEvent::ThinkingDelta { thinking, redacted } => {
                Some(StreamChunk::ThinkingDelta {
                    text: thinking.clone(),
                    redacted: redacted.clone(),
                })
            }
            AgentStreamEvent::ToolResult { .. } => None,
            AgentStreamEvent::Usage(_) => None,
            AgentStreamEvent::ResponseComplete => None,
        }
    }
}

// ─── transform: ProviderEvent → AgentStreamEvent ───────────────

/// 将 Provider 协议事件转换为 Agent 语义事件。
///
/// 支持 1:N 映射（一个 ProviderEvent 可能产生多个 AgentStreamEvent）。
/// 例如 `ResponseComplete` 同时携带 tool_calls 和 usage。
pub fn transform(event: &ProviderEvent) -> Vec<AgentStreamEvent> {
    match event {
        ProviderEvent::Token { token } => {
            vec![AgentStreamEvent::TextDelta {
                text: token.clone(),
            }]
        }
        ProviderEvent::ThinkingDelta { thinking, redacted } => {
            vec![AgentStreamEvent::ThinkingDelta {
                thinking: thinking.clone(),
                redacted: redacted.clone(),
            }]
        }
        ProviderEvent::ResponseComplete { tool_calls, usage } => {
            let mut events = Vec::new();
            // tool_calls 不在此处转换为 AgentStreamEvent，
            // 由 LLMNode 直接从 ProviderEvent 中提取。
            let _ = tool_calls; // 目前不转换，供未来扩展
            if let Some(u) = usage {
                events.push(AgentStreamEvent::Usage(*u));
            }
            events.push(AgentStreamEvent::ResponseComplete);
            events
        }
        ProviderEvent::Start { .. } => vec![], // 协议事件，忽略
    }
}

// ─── 向后兼容：TranslationResult ────────────────────────────────

/// ProviderEvent 转换结果（v0.4 API，向后兼容）。
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
    Usage(TokenUsage),
    /// 响应完成标记
    Finished,
    /// 忽略此事件（如 HeadersReceived, Start 等协议事件）
    Ignore,
}

/// 将 ProviderEvent 转换为 TranslationResult（v0.4 API，向后兼容）。
///
/// 内部通过 `transform()` + `to_chunk()` 实现。
/// 保留此接口以兼容现有 LLMNode 代码。
pub fn translate_provider_event(event: &ProviderEvent) -> TranslationResult {
    // 使用新的三层模型
    for agent_event in transform(event) {
        match agent_event {
            AgentStreamEvent::TextDelta { text } => {
                return TranslationResult::EmitWithText {
                    chunk: StreamChunk::TextDelta(text.clone()),
                    delta: text,
                };
            }
            AgentStreamEvent::ThinkingDelta { thinking, redacted } => {
                return TranslationResult::EmitWithThinking {
                    chunk: StreamChunk::ThinkingDelta {
                        text: thinking.clone(),
                        redacted: redacted.clone(),
                    },
                    delta: thinking,
                    redacted,
                };
            }
            AgentStreamEvent::Usage(u) => return TranslationResult::Usage(u),
            AgentStreamEvent::ResponseComplete => return TranslationResult::Finished,
            AgentStreamEvent::ToolResult { .. } => {}
        }
    }
    TranslationResult::Ignore
}

// ─── 测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── transform 测试 ──

    #[test]
    fn test_transform_token() {
        let event = ProviderEvent::Token {
            token: "hello".into(),
        };
        let result = transform(&event);
        assert_eq!(result.len(), 1);
        match &result[0] {
            AgentStreamEvent::TextDelta { text } => assert_eq!(text, "hello"),
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_thinking_with_redacted() {
        let event = ProviderEvent::ThinkingDelta {
            thinking: "visible".into(),
            redacted: Some("sensitive".into()),
        };
        let result = transform(&event);
        assert_eq!(result.len(), 1);
        match &result[0] {
            AgentStreamEvent::ThinkingDelta { thinking, redacted } => {
                assert_eq!(thinking, "visible");
                assert_eq!(redacted, &Some("sensitive".into()));
            }
            other => panic!("expected ThinkingDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_thinking_without_redacted() {
        let event = ProviderEvent::ThinkingDelta {
            thinking: "thinking".into(),
            redacted: None,
        };
        let result = transform(&event);
        assert_eq!(result.len(), 1);
        match &result[0] {
            AgentStreamEvent::ThinkingDelta { thinking, redacted } => {
                assert_eq!(thinking, "thinking");
                assert!(redacted.is_none());
            }
            other => panic!("expected ThinkingDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_transform_response_complete_with_usage() {
        let event = ProviderEvent::ResponseComplete {
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
        };
        let result = transform(&event);
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0], AgentStreamEvent::Usage(_)));
        assert!(matches!(result[1], AgentStreamEvent::ResponseComplete));
    }

    #[test]
    fn test_transform_response_complete_without_usage() {
        let event = ProviderEvent::ResponseComplete {
            tool_calls: vec![],
            usage: None,
        };
        let result = transform(&event);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], AgentStreamEvent::ResponseComplete));
    }

    #[test]
    fn test_transform_start_ignored() {
        let event = ProviderEvent::Start {
            model: "test".into(),
        };
        let result = transform(&event);
        assert!(result.is_empty());
    }

    // ── to_chunk 测试 ──

    #[test]
    fn test_to_chunk_text_delta() {
        let event = AgentStreamEvent::TextDelta {
            text: "hello".into(),
        };
        let chunk = event.to_chunk();
        assert!(chunk.is_some());
        match chunk.unwrap() {
            StreamChunk::TextDelta(t) => assert_eq!(t, "hello"),
            other => panic!("expected TextDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_to_chunk_thinking_delta() {
        let event = AgentStreamEvent::ThinkingDelta {
            thinking: "think".into(),
            redacted: Some("secret".into()),
        };
        let chunk = event.to_chunk();
        assert!(chunk.is_some());
        match chunk.unwrap() {
            StreamChunk::ThinkingDelta { text, redacted } => {
                assert_eq!(text, "think");
                assert_eq!(redacted, Some("secret".into()));
            }
            other => panic!("expected ThinkingDelta, got {:?}", other),
        }
    }

    #[test]
    fn test_to_chunk_usage_none() {
        let event = AgentStreamEvent::Usage(TokenUsage::default());
        assert!(event.to_chunk().is_none());
    }

    #[test]
    fn test_to_chunk_response_complete_none() {
        let event = AgentStreamEvent::ResponseComplete;
        assert!(event.to_chunk().is_none());
    }

    // ── 向后兼容测试 ──

    #[test]
    fn test_translate_provider_event_compat() {
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
