//! 迭代辅助函数 — execute() 与 execute_stream() 共享的工具箱。
//!
//! 包含带 Fallback 的重试执行器、流式事件发射、工具串行执行等。
//!
//! **设计笔记：** 曾尝试提取 `Iteration` 结构体封装单轮迭代流程，
//! 但 Rust borrow checker 不允许 `&mut state` 与 `state.iterations` 共存。
//! 未来可探索 `split_mut` 或 `cell` 模式。

use lellm_core::{ChatResponse, LlmError, Message};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use super::context::ContextBudget;
use super::event::AgentEvent;
use super::fallback::{FallbackAction, FallbackContext, FallbackStrategy};
use super::tools::ToolExecutor;

// ─── 带 Fallback 的执行器 ────────────────────────────────────────

/// 带 Fallback 重试的通用操作执行器。
///
/// **职责划分：**
/// - `FallbackContext` = 观察窗口（借用 `&LlmError`）
/// - Retry Loop = 错误所有者（Abort 时直接返回 owned `err`）
pub async fn execute_with_fallback<T, F, Fut>(
    fallback: &Arc<dyn FallbackStrategy>,
    mut op: F,
    iteration: usize,
    messages: &[Message],
) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, LlmError>>,
{
    let mut attempt: usize = 1;

    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                tracing::warn!(
                    attempt = attempt,
                    error = %err,
                    "provider operation failed, fallback handling"
                );
                let ctx = FallbackContext {
                    error: &err,
                    attempt,
                    iterations: iteration,
                    conversation: messages.to_vec().into(),
                };
                match fallback.handle(&ctx).await {
                    FallbackAction::Retry => {
                        attempt += 1;
                    }
                    FallbackAction::Abort => {
                        return Err(err);
                    }
                }
            }
        }
    }
}

// ─── 流式辅助 ────────────────────────────────────────────────────

/// 发送事件，消费者丢弃 Receiver 时返回 `false`。
pub async fn emit(tx: &Sender<AgentEvent>, event: AgentEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// 流式模式下 emit ToolStart/ToolEnd 并串行执行工具。
///
/// **设计决策（见 docs/DESIGN.md §8）：** 流式模式工具执行强制串行，
/// 即使工具标记为 Safe。原因：ToolStart/ToolEnd 与 Token 交错会让消费者解析更复杂。
/// v0.2 再优化流式分组并发。
pub async fn emit_and_execute_tools(
    tx: &Sender<AgentEvent>,
    executor: &ToolExecutor,
    tool_calls: &[lellm_core::ToolCall],
    budget: &ContextBudget,
) -> Option<Vec<Message>> {
    let mut results = Vec::new();

    for tc in tool_calls {
        if !emit(
            tx,
            AgentEvent::ToolStart {
                tool_call_id: tc.id.clone(),
                name: tc.name.clone(),
            },
        )
        .await
        {
            return None;
        }

        let raw_result = executor.execute(tc).await;

        // 工具结果截断
        let truncated_result = match &raw_result {
            Ok(text) => {
                let truncated = budget.truncate_tool_result(text.clone());
                if truncated != *text {
                    Ok(truncated)
                } else {
                    raw_result
                }
            }
            Err(_) => raw_result, // 错误消息不截断
        };

        if !emit(
            tx,
            AgentEvent::ToolEnd {
                tool_call_id: tc.id.clone(),
                result: truncated_result.clone(),
            },
        )
        .await
        {
            return None;
        }

        results.push(Message::tool_result(tc, &truncated_result));
    }

    Some(results)
}

/// 从累积的 buffer 构建部分 ChatResponse（输出预算超限时使用）
pub fn build_partial_response(
    text_buffer: String,
    thinking_buffer: String,
    redacted_buffer: Option<String>,
) -> ChatResponse {
    let mut content: Vec<lellm_core::ContentBlock> = Vec::new();

    if !thinking_buffer.is_empty() {
        content.push(lellm_core::ContentBlock::Thinking(
            lellm_core::ThinkingBlock {
                thinking: thinking_buffer,
                redacted: redacted_buffer,
            },
        ));
    }

    if !text_buffer.is_empty() {
        content.push(lellm_core::ContentBlock::Text(lellm_core::TextBlock {
            text: text_buffer,
        }));
    }

    if content.is_empty() {
        content.push(lellm_core::ContentBlock::Text(lellm_core::TextBlock {
            text: String::new(),
        }));
    }

    ChatResponse::new(
        content,
        lellm_core::TokenUsage::default(),
        serde_json::json!(null),
    )
}
