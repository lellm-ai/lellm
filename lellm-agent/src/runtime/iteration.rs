//! 迭代辅助函数 — execute() 与 execute_stream() 共享的工具箱。
//!
//! 包含带 Fallback 的重试执行器、流式迭代、工具串行执行等。

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message, ToolCall, ToolError};
use lellm_graph::State;
use lellm_provider::ResolvedModel;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use super::context::{AgentExecutionContext, ContextBudget, estimate_text};
use super::event::AgentEvent;
use super::retry::RetryPolicy;
use super::runtime::{
    ResolvedRound, build_result, get_iterations, state_add_output_from_content,
    state_add_tool_calls, state_push_assistant, state_push_tool_results,
};
use super::tools::{ToolExecutor, ToolRegistration, ToolSnapshot};

/// 工具映射类型别名
type ToolMap = indexmap::IndexMap<String, ToolRegistration>;

// ─── 流式辅助 ────────────────────────────────────────────────────

/// 发送事件，消费者丢弃 Receiver 时返回 `false`。
pub async fn emit(tx: &Sender<AgentEvent>, event: AgentEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// 流式模式下 emit ToolStart/ToolEnd 并分组并发执行工具（使用快照）。
///
/// **并发策略：** 复用 `execute_batch_with()` 的 `ParallelSafety` 分组逻辑：
/// - `Safe`: 全并发
/// - `CategoryExclusive`: 组内串行，组间并发
/// - `Exclusive`: 全串行
///
/// **事件顺序：** ToolStart 按原始顺序发出，ToolEnd 允许乱序。
/// 消费者用 `tool_call_id` 配对 Start/End。
///
/// **工具结果截断**统一在 `state_push_tool_results()` 中执行，此处不截断。
pub(super) async fn emit_and_execute_tools_with(
    tx: &Sender<AgentEvent>,
    snapshot: &ToolSnapshot,
    retry_policy: &RetryPolicy,
    tool_calls: &[ToolCall],
) -> Option<Vec<Message>> {
    if tool_calls.is_empty() {
        return Some(Vec::new());
    }

    // 1. 按原始顺序发出所有 ToolStart
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
    }

    // 2. 按 ParallelSafety 分组（与 execute_batch_with 相同逻辑）
    let mut safe_calls: Vec<(usize, ToolCall)> = Vec::new();
    let mut category_calls: std::collections::HashMap<
        super::tools::ToolCategory,
        Vec<(usize, ToolCall)>,
    > = std::collections::HashMap::new();
    let mut exclusive_calls: Vec<(usize, ToolCall)> = Vec::new();

    for (idx, call) in tool_calls.iter().enumerate() {
        let safety = snapshot
            .get(&call.name)
            .map(|t| t.safety.clone())
            .unwrap_or(super::tools::ParallelSafety::Exclusive);

        match safety {
            super::tools::ParallelSafety::Safe => safe_calls.push((idx, call.clone())),
            super::tools::ParallelSafety::CategoryExclusive => {
                if let Some(cat) = snapshot.get(&call.name).and_then(|t| t.category.clone()) {
                    category_calls
                        .entry(cat)
                        .or_default()
                        .push((idx, call.clone()));
                } else {
                    exclusive_calls.push((idx, call.clone()));
                }
            }
            super::tools::ParallelSafety::Exclusive => exclusive_calls.push((idx, call.clone())),
        }
    }

    // 3. 分组并发执行，每个完成后立即 emit ToolEnd
    let snapshot_arc: Arc<ToolMap> = snapshot.clone_for_spawn();
    let retry_policy = retry_policy.clone();
    let tx = tx.clone();

    let mut group_handles: Vec<tokio::task::JoinHandle<Vec<(usize, Message)>>> = Vec::new();

    // Safe: 每个 tool 独立 spawn（全并发）
    if !safe_calls.is_empty() {
        let s = Arc::clone(&snapshot_arc);
        let rp = retry_policy.clone();
        let tx_clone = tx.clone();
        group_handles.push(tokio::spawn(async move {
            let handles: Vec<_> = safe_calls
                .iter()
                .map(|(idx, call)| {
                    let tools = Arc::clone(&s);
                    let rp = rp.clone();
                    let call = call.clone();
                    let idx = *idx;
                    let tx = tx_clone.clone();
                    tokio::spawn(async move {
                        let result = match tools.get(&call.name) {
                            Some(entry) => {
                                rp.execute_with_retry(&entry.func, &call.arguments).await
                            }
                            None => {
                                Err(ToolError::not_found(format!("unknown tool: {}", call.name)))
                            }
                        };
                        let _ = emit(
                            &tx,
                            AgentEvent::ToolEnd {
                                tool_call_id: call.id.clone(),
                                result: result.clone(),
                            },
                        )
                        .await;
                        (idx, Message::tool_result(&call, &result))
                    })
                })
                .collect();

            let raw = futures_util::future::join_all(handles).await;
            raw.into_iter()
                .map(|h| match h {
                    Ok((idx, msg)) => (idx, msg),
                    Err(join_err) => {
                        panic!("tool task panicked: {join_err}");
                    }
                })
                .collect()
        }));
    }

    // CategoryExclusive: 按 category 分组，组内串行、组间并发
    for group_calls in category_calls.into_values() {
        let s = Arc::clone(&snapshot_arc);
        let rp = retry_policy.clone();
        let tx_clone = tx.clone();
        group_handles.push(tokio::spawn(async move {
            let mut results = Vec::with_capacity(group_calls.len());
            for (idx, call) in group_calls {
                let result = match s.get(&call.name) {
                    Some(entry) => rp.execute_with_retry(&entry.func, &call.arguments).await,
                    None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
                };
                let _ = emit(
                    &tx_clone,
                    AgentEvent::ToolEnd {
                        tool_call_id: call.id.clone(),
                        result: result.clone(),
                    },
                )
                .await;
                results.push((idx, Message::tool_result(&call, &result)));
            }
            results
        }));
    }

    // Exclusive: 全部串行，一个 task
    if !exclusive_calls.is_empty() {
        let s = Arc::clone(&snapshot_arc);
        let rp = retry_policy.clone();
        let tx_clone = tx.clone();
        group_handles.push(tokio::spawn(async move {
            let mut results = Vec::with_capacity(exclusive_calls.len());
            for (idx, call) in exclusive_calls {
                let result = match s.get(&call.name) {
                    Some(entry) => rp.execute_with_retry(&entry.func, &call.arguments).await,
                    None => Err(ToolError::not_found(format!("unknown tool: {}", call.name))),
                };
                let _ = emit(
                    &tx_clone,
                    AgentEvent::ToolEnd {
                        tool_call_id: call.id.clone(),
                        result: result.clone(),
                    },
                )
                .await;
                results.push((idx, Message::tool_result(&call, &result)));
            }
            results
        }));
    }

    // 4. 等待所有组完成，按原始索引回填结果
    let mut results: Vec<Option<Message>> = vec![None; tool_calls.len()];
    let all_handles = futures_util::future::join_all(group_handles).await;

    for indexed_messages in all_handles.into_iter().filter_map(Result::ok) {
        for (idx, msg) in indexed_messages {
            results[idx] = Some(msg);
        }
    }

    Some(results.into_iter().flatten().collect())
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
            cache_control: None,
        }));
    }

    if content.is_empty() {
        content.push(lellm_core::ContentBlock::Text(lellm_core::TextBlock {
            text: String::new(),
            cache_control: None,
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
#[allow(clippy::too_many_arguments)]
async fn process_stream_iteration(
    tx: &Sender<AgentEvent>,
    executor: &ToolExecutor,
    state: &mut State,
    ctx: &mut AgentExecutionContext,
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
                if let Some(limit) =
                    max_reasoning_tokens.filter(|&limit| (round_reasoning_tokens as u32) > limit)
                {
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
        if matches!(&ev, lellm_provider::ProviderEvent::ThinkingDelta { .. }) && !stream_thinking {
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
                    cache_control: None,
                }));
            }

            content.extend(
                pending_tool_calls
                    .iter()
                    .map(|tc| lellm_core::ContentBlock::ToolCall(tc.clone())),
            );

            let response = ChatResponse::new(content, usage_val, serde_json::json!(null));

            if !pending_tool_calls.is_empty() {
                state_push_assistant(state, ctx, response.content.clone());
                state_add_output_from_content(state, ctx, &response.content);
                state_add_tool_calls(state, pending_tool_calls.len());

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
                state_push_tool_results(state, ctx, results.unwrap(), budget);

                tracing::debug!(
                    iteration = get_iterations(state),
                    tool_calls = pending_tool_calls.len(),
                    "tool-use stream iteration"
                );

                return Ok(StreamIterResult::Continue { response });
            } else {
                state_add_output_from_content(state, ctx, &response.content);

                if !emit(
                    tx,
                    AgentEvent::LoopEnd {
                        result: build_result(
                            state,
                            super::event::StopReason::Complete,
                            response.clone(),
                        ),
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
    pub(super) result: Result<(StreamIterResult, State), LlmError>,
    pub(super) ctx: AgentExecutionContext,
    pub(super) stream_started: bool,
}

/// 执行单次流式迭代：打开 stream → 消费事件 → 处理工具调用。
///
/// 接收 owned 参数（Arc-clone 为 O(1)），避免闭包捕获带来的变量爆炸。
/// 返回迭代结果和更新后的 State + AgentExecutionContext。
#[allow(clippy::too_many_arguments)]
pub(super) async fn do_stream_iteration(
    model: ResolvedModel,
    tx: Sender<AgentEvent>,
    executor: ToolExecutor,
    state: State,
    ctx: AgentExecutionContext,
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
                ctx,
                stream_started: false,
            };
        }
    };

    let mut text_buffer = String::new();
    let mut thinking_buffer = String::new();
    let mut redacted_buffer: Option<String> = None;
    let mut attempt_state = state;
    let mut attempt_ctx = ctx;

    // stream 已打开 → 事件可能已发出，失败后禁止 Retry
    let iter_result = process_stream_iteration(
        &tx,
        &executor,
        &mut attempt_state,
        &mut attempt_ctx,
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
        ctx: attempt_ctx,
        stream_started: true,
    }
}
