//! AgentEventSink — StreamChunk → AgentEvent 桥接。
//!
//! 实现 `StreamSink` trait，将 Graph 层的数据面事件（StreamChunk）
//! 转换为 Agent 层的控制面事件（AgentEvent），发送到 mpsc channel。
//!
//! # 映射关系
//!
//! | StreamChunk | AgentEvent |
//! |---|---|
//! | TextDelta | Provider(Token) |
//! | ThinkingDelta | Provider(ThinkingDelta) |
//! | ToolLifecycle(Started) | ToolStart |
//! | ToolOutput | ToolEnd |
//! | ToolLifecycle(Queued/Finished) | 忽略 |
//!
//! # 设计原则
//!
//! - 同步 emit，永不阻塞（try_send）
//! - 消费者断开时静默丢弃（cancel token 会传播）
//! - 不缓存历史事件，只做实时转换

use lellm_core::{ToolError, ToolErrorKind, ToolResult};
use lellm_graph::{StreamChunk, StreamSink, ToolPhase};
use lellm_provider::ProviderEvent;
use tokio::sync::mpsc;

use super::event::AgentEvent;

/// AgentEventSink — 将 StreamChunk 转换为 AgentEvent 并发送到 channel。
///
/// 内部持有 `mpsc::Sender<AgentEvent>`，通过 `try_send` 实现非阻塞发射。
/// 消费者断开时，发送失败被静默忽略（由 cancel token 传播终止信号）。
#[derive(Clone)]
pub struct AgentEventSink {
    tx: mpsc::Sender<AgentEvent>,
}

impl AgentEventSink {
    /// 创建 AgentEventSink。
    ///
    /// # Arguments
    /// * `tx` - AgentEvent 的发送端（有界 channel）
    pub fn new(tx: mpsc::Sender<AgentEvent>) -> Self {
        Self { tx }
    }

    /// 发送 AgentEvent（非阻塞）。
    ///
    /// 返回 `true` 表示发送成功，`false` 表示消费者已断开。
    fn emit_event(&self, event: AgentEvent) -> bool {
        self.tx.try_send(event).is_ok()
    }
}

impl StreamSink for AgentEventSink {
    fn emit(&self, chunk: StreamChunk) {
        match chunk {
            StreamChunk::TextDelta(token) => {
                self.emit_event(AgentEvent::Provider(ProviderEvent::Token { token }));
            }
            StreamChunk::ThinkingDelta(thinking) => {
                self.emit_event(AgentEvent::Provider(ProviderEvent::ThinkingDelta {
                    thinking,
                    redacted: None,
                }));
            }
            StreamChunk::ToolLifecycle {
                phase,
                call_id,
                tool_name,
            } => match phase {
                ToolPhase::Queued => {
                    // Queued 不需要 emit — AgentEvent 没有对应的变体
                }
                ToolPhase::Started => {
                    self.emit_event(AgentEvent::ToolStart {
                        tool_call_id: call_id,
                        name: tool_name,
                    });
                }
                ToolPhase::Finished => {
                    // Finished 被 ToolOutput 覆盖，此处忽略
                    // ToolOutput 携带了完整的执行结果
                }
            },
            StreamChunk::ToolOutput {
                call_id,
                tool_name: _tool_name,
                content,
                is_error,
                duration: _duration,
            } => {
                let result: ToolResult = if is_error {
                    Err(ToolError {
                        kind: ToolErrorKind::Internal,
                        message: content,
                    })
                } else {
                    // 将 content 字符串解析为 json，失败则包装为字符串
                    match serde_json::from_str::<serde_json::Value>(&content) {
                        Ok(val) => Ok(val),
                        Err(_) => Ok(serde_json::json!(content)),
                    }
                };

                self.emit_event(AgentEvent::ToolEnd {
                    tool_call_id: call_id,
                    result,
                });
            }
        }
    }
}

// ─── 测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_delta_mapping() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::TextDelta("hello".into()));

        let event = rx.blocking_recv();
        assert!(matches!(
            event,
            Some(AgentEvent::Provider(ProviderEvent::Token { token }))
                if token == "hello"
        ));
    }

    #[test]
    fn test_thinking_delta_mapping() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::ThinkingDelta("thinking...".into()));

        let event = rx.blocking_recv();
        assert!(matches!(
            event,
            Some(AgentEvent::Provider(ProviderEvent::ThinkingDelta { thinking, .. }))
                if thinking == "thinking..."
        ));
    }

    #[test]
    fn test_tool_started_mapping() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::ToolLifecycle {
            phase: ToolPhase::Started,
            call_id: "call_1".into(),
            tool_name: "search".into(),
        });

        let event = rx.blocking_recv();
        assert!(matches!(
            event,
            Some(AgentEvent::ToolStart { tool_call_id, name })
                if tool_call_id == "call_1" && name == "search"
        ));
    }

    #[test]
    fn test_tool_output_success_mapping() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::ToolOutput {
            call_id: "call_1".into(),
            tool_name: "search".into(),
            content: r#"{"results": ["a", "b"]}"#.into(),
            is_error: false,
            duration: std::time::Duration::from_millis(100),
        });

        let event = rx.blocking_recv();
        match event {
            Some(AgentEvent::ToolEnd {
                tool_call_id,
                result,
            }) => {
                assert_eq!(tool_call_id, "call_1");
                assert!(result.is_ok());
            }
            other => panic!("expected ToolEnd, got {:?}", other),
        }
    }

    #[test]
    fn test_tool_output_error_mapping() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::ToolOutput {
            call_id: "call_2".into(),
            tool_name: "search".into(),
            content: "not found".into(),
            is_error: true,
            duration: std::time::Duration::from_millis(50),
        });

        let event = rx.blocking_recv();
        match event {
            Some(AgentEvent::ToolEnd {
                tool_call_id,
                result,
            }) => {
                assert_eq!(tool_call_id, "call_2");
                assert!(result.is_err());
            }
            other => panic!("expected ToolEnd, got {:?}", other),
        }
    }

    #[test]
    fn test_tool_queued_ignored() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::ToolLifecycle {
            phase: ToolPhase::Queued,
            call_id: "call_1".into(),
            tool_name: "search".into(),
        });

        // Queued 被忽略，channel 应为空
        assert!(rx.blocking_recv().is_none());
    }

    #[test]
    fn test_tool_finished_ignored() {
        let (tx, mut rx) = mpsc::channel(32);
        let sink = AgentEventSink::new(tx);

        sink.emit(StreamChunk::ToolLifecycle {
            phase: ToolPhase::Finished,
            call_id: "call_1".into(),
            tool_name: "search".into(),
        });

        // Finished 被忽略，channel 应为空
        assert!(rx.blocking_recv().is_none());
    }
}
