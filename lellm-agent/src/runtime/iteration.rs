//! 迭代辅助函数 — execute() 与 execute_stream() 共享的工具箱。
//!
//! 包含带 Fallback 的重试执行器、流式迭代、工具串行执行等。

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message, ToolCall, ToolError};
use lellm_provider::ResolvedModel;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use super::LoopState;
use super::runtime::ResolvedRound;
use super::context::{ContextBudget, estimate_text};
use super::event::AgentEvent;
use super::fallback::{FallbackAction, FallbackContext, FallbackStrategy};
use super::retry::RetryPolicy;
use super::tools::{ToolExecutor, ToolSnapshot};

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

/// 流式模式下 emit ToolStart/ToolEnd 并串行执行工具（使用快照）。
///
/// **设计决策：** 流式模式工具执行强制串行，
/// 即使工具标记为 Safe。原因：ToolStart/ToolEnd 与 Token 交错会让消费者解析更复杂。
/// v0.2 再优化流式分组并发。
///
/// **工具结果截断**统一在 `LoopState.push_tool_results()` 中执行，此处不截断。
pub(super) async fn emit_and_execute_tools_with(
    tx: &Sender<AgentEvent>,
    snapshot: &ToolSnapshot,
    retry_policy: &RetryPolicy,
    tool_calls: &[ToolCall],
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

        let raw_result = match snapshot.get(&tc.name) {
            Some(entry) => retry_policy
                .execute_with_retry_and_emission(&entry.func, &tc.arguments, tx, &tc.id)
                .await,
            None => Err(ToolError::not_found(format!("unknown tool: {}", tc.name))),
        };

        if !emit(
            tx,
            AgentEvent::ToolEnd {
                tool_call_id: tc.id.clone(),
                result: raw_result.clone(),
            },
        )
        .await
        {
            return None;
        }

        results.push(Message::tool_result(tc, &raw_result));
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

// ─── 流式单轮迭代结果 ─────────────────────────────────────────────

/// 流式单轮迭代的合法结果 — 枚举保证类型安全。
///
/// **设计原则：** 仅表达"一次迭代成功完成后的状态"。
/// 错误通过 `Result<StreamIterResult, LlmError>` 的 `Err` 表达。
#[must_use]
pub(super) enum StreamIterResult {
    /// 继续循环（有 tool_calls，应进入下一轮）
    Continue { response: ChatResponse },
    /// 正常完成（无 tool_calls，Agent 已获得最终答案）
    Complete { response: ChatResponse },
    /// 消费者断开（不再继续）
    Cancelled { response: Option<ChatResponse> },
    /// 单轮输出预算超限（Provider 忽略 max_tokens 或模型无限输出）
    OutputBudgetExceeded { response: ChatResponse },
    /// 单轮推理预算超限（Provider 忽略 max_tokens 或模型疯狂推理）
    ReasoningBudgetExceeded { response: ChatResponse },
}

// ─── 流式单轮迭代（含 stream 打开）────────────────────────────────

/// 处理流式单轮迭代（不含打开 stream）。
///
/// 返回 `Ok(StreamIterResult)` 表示迭代完成，`Err(LlmError)` 表示 Provider 错误。
///
/// `round` — 预解析的 ResolvedRound（快照 + 定义），充当单轮真理之源。
async fn process_stream_iteration(
    tx: &Sender<AgentEvent>,
    executor: &ToolExecutor,
    state: &mut LoopState,
    stream: &mut lellm_provider::ProviderStream,
    text_buffer: &mut String,
    thinking_buffer: &mut String,
    redacted_buffer: &mut Option<String>,
    budget: &ContextBudget,
    max_output_tokens: u32,
    max_reasoning_tokens: Option<u32>,
    stream_thinking: bool,
    round: ResolvedRound,
) -> Result<StreamIterResult, LlmError> {
    use futures_util::StreamExt;

    let mut round_output_tokens: usize = 0;
    let mut round_reasoning_tokens: usize = 0;

    while let Some(result) = stream.next().await {
        let ev = match result {
            Ok(ev) => ev,
            Err(e) => return Err(e),
        };

        // 统一透传 Provider 事件 — Provider 发一次，Agent 只负责转发
        match &ev {
            lellm_provider::ProviderEvent::Token { token } => {
                round_output_tokens += estimate_text(token);
                if (round_output_tokens as u32) > max_output_tokens {
                    tracing::warn!(
                        round_output_tokens,
                        max_output_tokens,
                        "single-round output budget exceeded, cutting stream"
                    );
                    let response = build_partial_response(
                        text_buffer.clone(),
                        thinking_buffer.clone(),
                        redacted_buffer.clone(),
                    );
                    return Ok(StreamIterResult::OutputBudgetExceeded { response });
                }
                text_buffer.push_str(token);
            }
            lellm_provider::ProviderEvent::ThinkingDelta { thinking, redacted } => {
                round_reasoning_tokens += estimate_text(thinking)
                    + redacted.as_ref().map(|r| estimate_text(r)).unwrap_or(0);
                if let Some(limit) = max_reasoning_tokens {
                    if (round_reasoning_tokens as u32) > limit {
                        tracing::warn!(
                            round_reasoning_tokens,
                            max_reasoning_tokens = limit,
                            "single-round reasoning budget exceeded, cutting stream"
                        );
                        let response = build_partial_response(
                            text_buffer.clone(),
                            thinking_buffer.clone(),
                            redacted_buffer.clone(),
                        );
                        return Ok(StreamIterResult::ReasoningBudgetExceeded { response });
                    }
                }
                thinking_buffer.push_str(thinking);
                if let Some(r) = redacted {
                    if let Some(ref mut prev) = *redacted_buffer {
                        prev.push_str(r);
                    } else {
                        *redacted_buffer = Some(r.clone());
                    }
                }
            }
            lellm_provider::ProviderEvent::Start { .. }
            | lellm_provider::ProviderEvent::ResponseComplete { .. } => {}
        }

        // ThinkingDelta 根据 stream_thinking 决定是否向消费者发射。
        // 注意：预算检查和累积始终执行，不受 stream_thinking 影响。
        if matches!(&ev, lellm_provider::ProviderEvent::ThinkingDelta { .. })
            && !stream_thinking
        {
            // 跳过 ThinkingDelta 发射，继续下一事件
        } else if !emit(tx, AgentEvent::Provider(ev.clone())).await {
            return Ok(StreamIterResult::Cancelled { response: None });
        }

        // ResponseComplete 事件需要特殊处理：工具执行、终止判断
        if let lellm_provider::ProviderEvent::ResponseComplete { tool_calls, usage } = ev {
            let pending_tool_calls = tool_calls;
            let usage_val = usage.unwrap_or_default();

            // 统一构建 ChatResponse
            let mut content: Vec<lellm_core::ContentBlock> = Vec::new();

            if !thinking_buffer.is_empty() {
                content.push(lellm_core::ContentBlock::Thinking(
                    lellm_core::ThinkingBlock {
                        thinking: thinking_buffer.clone(),
                        redacted: redacted_buffer.clone(),
                    },
                ));
            }

            if !text_buffer.is_empty() {
                content.push(lellm_core::ContentBlock::Text(lellm_core::TextBlock {
                    text: text_buffer.clone(),
                }));
            }

            content.extend(
                pending_tool_calls
                    .iter()
                    .map(|tc| lellm_core::ContentBlock::ToolCall(tc.clone())),
            );

            let response = ChatResponse::new(content, usage_val, serde_json::json!(null));

            if !pending_tool_calls.is_empty() {
                state.push_assistant(response.content.clone());
                state.add_output_from_content(&response.content);
                state.add_tool_calls(pending_tool_calls.len());

                let results = emit_and_execute_tools_with(
                    tx,
                    &round.snapshot,
                    &executor.retry_policy(),
                    &pending_tool_calls,
                )
                .await;
                if results.is_none() {
                    return Ok(StreamIterResult::Cancelled {
                        response: Some(response),
                    });
                }
                state.push_tool_results(results.unwrap(), budget);

                tracing::debug!(
                    iteration = state.iterations,
                    tool_calls = pending_tool_calls.len(),
                    "tool-use stream iteration"
                );

                return Ok(StreamIterResult::Continue { response });
            } else {
                state.add_output_from_content(&response.content);

                if !emit(
                    tx,
                    AgentEvent::LoopEnd {
                        result: state.finish_complete(response.clone()),
                    },
                )
                .await
                {
                    return Ok(StreamIterResult::Cancelled {
                        response: Some(response),
                    });
                }

                return Ok(StreamIterResult::Complete { response });
            }
        }
    }

    Err(LlmError::UnexpectedEof)
}

/// 流式迭代结果，附带 stream_started 标记。
///
/// `stream_started = true` 表示 stream 已打开（可能已发出事件），
/// 此时失败禁止 Retry（防止重复 Token）。
pub(super) struct StreamIterationResult {
    pub(super) result: Result<(StreamIterResult, LoopState), LlmError>,
    pub(super) stream_started: bool,
}

/// 执行单次流式迭代：打开 stream → 消费事件 → 处理工具调用。
///
/// 接收 owned 参数（Arc-clone 为 O(1)），避免闭包捕获带来的变量爆炸。
/// 返回迭代结果和更新后的 LoopState。
pub(super) async fn do_stream_iteration(
    model: ResolvedModel,
    tx: Sender<AgentEvent>,
    executor: ToolExecutor,
    state: LoopState,
    req: ChatRequest,
    budget: ContextBudget,
    max_output_tokens: u32,
    stream_thinking: bool,
    round: ResolvedRound,
) -> StreamIterationResult {
    let max_reasoning_tokens = req.max_reasoning_tokens;

    // stream() 失败 → 未发出任何事件，允许 Retry
    let mut stream = match model.provider.stream(&req).await {
        Ok(s) => s,
        Err(e) => {
            return StreamIterationResult {
                result: Err(e),
                stream_started: false,
            };
        }
    };

    let mut text_buffer = String::new();
    let mut thinking_buffer = String::new();
    let mut redacted_buffer: Option<String> = None;
    let mut attempt_state = state;

    // stream 已打开 → 事件可能已发出，失败后禁止 Retry
    let iter_result = process_stream_iteration(
        &tx,
        &executor,
        &mut attempt_state,
        &mut stream,
        &mut text_buffer,
        &mut thinking_buffer,
        &mut redacted_buffer,
        &budget,
        max_output_tokens,
        max_reasoning_tokens,
        stream_thinking,
        round,
    )
    .await;

    StreamIterationResult {
        result: iter_result.map(|r| (r, attempt_state)),
        stream_started: true,
    }
}
