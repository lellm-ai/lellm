//! Agent Loop — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。
//!
//! # 架构分层
//!
//! ```text
//! ToolUseLoop
//! ├── model:       ResolvedModel     (Provider + model name)
//! ├── executor:    ToolExecutor      (ToolCatalog + 执行引擎)
//! ├── config:      ToolUseConfig     (纯参数, Clone + Send + Sync)
//! └── deps:        ToolUseDeps       (策略服务, Arc 包裹)
//! ```

use lellm_core::{ChatResponse, LlmError, Message};
use lellm_graph::{
    SK_ITERATIONS, SK_MESSAGES, SK_OUTPUT_TOKENS, SK_REASONING_TOKENS, SK_TOTAL_TOOL_CALLS, State,
    StateKeyExt,
};
use lellm_provider::ResolvedModel;
use std::sync::Arc;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_messages_inner, empty_response};
use super::context::{
    AgentExecutionContext, CompactionResult, ContextBudget, ContextCompactor, LocalCompactor,
    estimate_reasoning_block, estimate_text, estimate_tokens,
};
use super::event::{AgentEvent, AgentStream, StopReason};
use super::fallback::{FallbackAction, FallbackContext};
use super::iteration::{StreamIterResult, do_stream_iteration, emit, execute_with_fallback};
use super::tools::{ToolExecutor, ToolSnapshot, execute_batch_with};

// ─── 本轮解析数据 ────────────────────────────────────────────────

/// 本轮对话锁定的快照 + 定义。
///
/// 一旦创建，内容不再变化。充当单轮的"真理之源"。
#[derive(Clone)]
pub struct ResolvedRound {
    /// 本轮对话锁定的快照
    pub snapshot: Arc<ToolSnapshot>,
    /// 为当前 LLM 供给的工具定义（已在前置阶段从快照中提取并平铺）
    pub definitions: Vec<lellm_core::ToolDefinition>,
}

impl ResolvedRound {
    pub fn new(snapshot: Arc<ToolSnapshot>) -> Self {
        Self {
            definitions: snapshot.definitions().to_vec(),
            snapshot,
        }
    }
}

// ─── State 辅助函数 ─────────────────────────────────────────────

/// 创建初始 State，写入消息列表和计数器。
pub(crate) fn create_initial_state(messages: &[Message]) -> State {
    let mut state = State::new();
    let messages_json: Vec<serde_json::Value> = messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    state.set_sk(&SK_MESSAGES, messages_json);
    state.set_sk(&SK_ITERATIONS, 0u32);
    state.set_sk(&SK_TOTAL_TOOL_CALLS, 0usize);
    state.set_sk(&SK_OUTPUT_TOKENS, 0usize);
    state.set_sk(&SK_REASONING_TOKENS, 0usize);
    state
}

/// 从 State 读取消息列表（反序列化）。
pub(crate) fn get_messages(state: &State) -> Vec<Message> {
    state
        .get_sk::<Vec<serde_json::Value>>(&SK_MESSAGES)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// 从 State 读取迭代轮次。
pub(crate) fn get_iterations(state: &State) -> u32 {
    state.get_sk::<u32>(&SK_ITERATIONS).unwrap_or(0)
}

/// 从 State 读取累计工具调用数。
pub(crate) fn get_tool_calls(state: &State) -> usize {
    state.get_sk::<usize>(&SK_TOTAL_TOOL_CALLS).unwrap_or(0)
}

/// 从 State 读取累计输出 Token 数。
pub(crate) fn get_output_tokens(state: &State) -> usize {
    state.get_sk::<usize>(&SK_OUTPUT_TOKENS).unwrap_or(0)
}

/// 从 State 读取累计推理 Token 数。
pub(crate) fn get_reasoning_tokens(state: &State) -> usize {
    state.get_sk::<usize>(&SK_REASONING_TOKENS).unwrap_or(0)
}

/// 追加 Assistant 响应到消息历史。
pub(crate) fn state_push_assistant(
    state: &mut State,
    ctx: &mut AgentExecutionContext,
    content: Vec<lellm_core::ContentBlock>,
) {
    let msg = Message::Assistant {
        content: content.clone(),
    };
    let tokens = estimate_tokens(&[msg]);
    ctx.add_tokens(tokens);
    let mut messages_json: Vec<serde_json::Value> = state.get_sk(&SK_MESSAGES).unwrap_or_default();
    messages_json.push(serde_json::to_value(Message::Assistant { content }).unwrap_or_default());
    state.set_sk(&SK_MESSAGES, messages_json);
}

/// 追加工具执行结果到消息历史。
pub(crate) fn state_push_tool_results(
    state: &mut State,
    ctx: &mut AgentExecutionContext,
    results: Vec<Message>,
    budget: &ContextBudget,
) {
    let results: Vec<Message> = results
        .into_iter()
        .map(|m| {
            if let Message::ToolResult {
                ref tool_call_id,
                is_error: false,
                ref content,
            } = m
            {
                let truncated = budget.truncate_tool_result_blocks(content);
                if truncated != *content {
                    return Message::ToolResult {
                        tool_call_id: tool_call_id.clone(),
                        is_error: false,
                        content: truncated,
                    };
                }
            }
            m
        })
        .collect();
    let tokens = estimate_tokens(&results);
    ctx.add_tokens(tokens);
    let mut messages_json: Vec<serde_json::Value> = state.get_sk(&SK_MESSAGES).unwrap_or_default();
    for msg in results {
        messages_json.push(serde_json::to_value(msg).unwrap_or_default());
    }
    state.set_sk(&SK_MESSAGES, messages_json);
}

/// 记录本轮工具调用数量。
pub(crate) fn state_add_tool_calls(state: &mut State, count: usize) {
    let current = state.get_sk::<usize>(&SK_TOTAL_TOOL_CALLS).unwrap_or(0);
    state.set_sk(&SK_TOTAL_TOOL_CALLS, current + count);
}

/// 从 ContentBlock 分离估算 Output 和 Reasoning Token。
pub(crate) fn state_add_output_from_content(
    state: &mut State,
    ctx: &mut AgentExecutionContext,
    content: &[lellm_core::ContentBlock],
) {
    let mut output_tokens: usize = 0;
    let mut reasoning_tokens: usize = 0;
    for b in content {
        match b {
            lellm_core::ContentBlock::Text(t) => output_tokens += estimate_text(&t.text),
            lellm_core::ContentBlock::Thinking(th) => {
                reasoning_tokens += estimate_reasoning_block(th)
            }
            lellm_core::ContentBlock::Image { .. } | lellm_core::ContentBlock::ToolCall(_) => {}
        }
    }
    let current_output = state.get_sk::<usize>(&SK_OUTPUT_TOKENS).unwrap_or(0);
    state.set_sk(&SK_OUTPUT_TOKENS, current_output + output_tokens);
    let current_reasoning = state.get_sk::<usize>(&SK_REASONING_TOKENS).unwrap_or(0);
    state.set_sk(&SK_REASONING_TOKENS, current_reasoning + reasoning_tokens);
    ctx.add_tokens(output_tokens + reasoning_tokens);
}

/// 进入下一轮迭代。
pub(crate) fn state_next_iteration(state: &mut State) {
    let current = state.get_sk::<u32>(&SK_ITERATIONS).unwrap_or(0);
    state.set_sk(&SK_ITERATIONS, current + 1);
}

/// 判断是否已达到最大轮次。
pub(crate) fn state_reached_max(state: &State, max_iterations: usize) -> bool {
    get_iterations(state) >= max_iterations as u32
}

/// 对消息历史执行压缩。
pub(crate) fn state_compact(
    state: &mut State,
    ctx: &mut AgentExecutionContext,
    budget: &ContextBudget,
    compactor: &dyn ContextCompactor,
) -> Option<CompactionResult> {
    if !budget.should_compact(ctx.cached_token_count) {
        return None;
    }
    let messages = get_messages(state);
    let result = compactor.compact(&messages, budget);
    let messages_json: Vec<serde_json::Value> = result
        .messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    state.set_sk(&SK_MESSAGES, messages_json);
    ctx.cached_token_count = result.after_tokens;
    Some(result)
}

/// 检查是否超过总输出预算。
pub(crate) fn state_exceeded_total_output(state: &State, max: Option<u32>) -> bool {
    match max {
        Some(limit) => get_output_tokens(state) >= limit as usize,
        None => false,
    }
}

/// 检查是否超过总推理预算。
pub(crate) fn state_exceeded_total_reasoning(state: &State, max: Option<u32>) -> bool {
    match max {
        Some(limit) => get_reasoning_tokens(state) >= limit as usize,
        None => false,
    }
}

/// 构建最终执行结果。
pub(crate) fn build_result(
    state: &State,
    stop_reason: StopReason,
    response: ChatResponse,
) -> ToolUseResult {
    ToolUseResult {
        stop_reason,
        response,
        messages: get_messages(state),
        iterations: get_iterations(state) as usize,
        tool_calls_executed: get_tool_calls(state),
    }
}

// ─── 执行结果 ───────────────────────────────────────────────────

/// ToolUseLoop 执行结果
#[derive(Debug, Clone)]
pub struct ToolUseResult {
    pub stop_reason: StopReason,
    pub response: ChatResponse,
    pub messages: Vec<Message>,
    pub iterations: usize,
    pub tool_calls_executed: usize,
}

impl ToolUseResult {
    pub fn is_success(&self) -> bool {
        matches!(self.stop_reason, StopReason::Complete)
    }
}

// ─── ToolUseLoop ────────────────────────────────────────────────

/// 管理 LLM 与工具调用闭环。
///
/// 内部全为 Arc/Clone，clone 为 O(1)，支持并发 execute。
#[derive(Clone)]
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    config: ToolUseConfig,
    deps: ToolUseDeps,
}

impl ToolUseLoop {
    pub fn new(
        model: ResolvedModel,
        executor: ToolExecutor,
        config: ToolUseConfig,
        deps: ToolUseDeps,
    ) -> Self {
        if config.stream_thinking {
            let caps = model.provider.capabilities_for(&model.model);
            if !caps.supports_stream_thinking {
                tracing::warn!(
                    provider = %model.provider.provider_id(),
                    model = %model.model,
                    "stream_thinking=true but provider does not support thinking deltas; \
                     reasoning content will only be available in the final response"
                );
            }
        }

        Self {
            model,
            executor,
            config,
            deps,
        }
    }

    /// 便捷构造 — 使用默认配置和依赖。
    pub fn simple(model: ResolvedModel, executor: ToolExecutor) -> Self {
        Self::new(
            model,
            executor,
            ToolUseConfig::default(),
            ToolUseDeps::default(),
        )
    }

    /// 非流式执行
    pub async fn execute(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let initial_messages = build_request_messages_inner(&self.config, &messages)?;
        let mut state = create_initial_state(&initial_messages);
        let mut ctx = AgentExecutionContext::new(&initial_messages);
        let mut last_response: Option<ChatResponse> = None;
        let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());

        loop {
            if state_reached_max(&state, self.config.max_iterations) {
                return Ok(build_result(
                    &state,
                    StopReason::MaxIterationsReached,
                    last_response.unwrap_or_else(empty_response),
                ));
            }
            state_next_iteration(&mut state);
            state_compact(
                &mut state,
                &mut ctx,
                &self.config.context_budget,
                &*compactor,
            );

            let round = ResolvedRound::new(self.executor.snapshot().await);

            let req = super::config::build_request_inner_with_round(
                &self.model,
                &get_messages(&state),
                self.config.max_output_tokens,
                &self.config.request_options,
                get_iterations(&state) as usize,
                &round.definitions,
            );

            let iteration = get_iterations(&state) as usize;
            let msg_snapshot = get_messages(&state);
            let response = execute_with_fallback(
                &self.deps.fallback,
                |_| true,
                || self.model.provider.call(&req),
                iteration,
                &msg_snapshot,
            )
            .await?;
            last_response = Some(response.clone());

            if let Some(limit) = self.config.request_options.max_reasoning_tokens {
                let round_reasoning: usize = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        lellm_core::ContentBlock::Thinking(th) => {
                            Some(estimate_reasoning_block(th))
                        }
                        _ => None,
                    })
                    .sum();
                if round_reasoning > limit as usize {
                    tracing::warn!(
                        round_reasoning,
                        max_reasoning_tokens = limit,
                        "single-round reasoning budget exceeded (non-stream, soft limit)"
                    );
                    return Ok(build_result(
                        &state,
                        StopReason::ReasoningBudgetExceeded,
                        response,
                    ));
                }
            }

            state_add_output_from_content(&mut state, &mut ctx, &response.content);

            if state_exceeded_total_output(&state, self.config.max_total_output_tokens) {
                return Ok(build_result(
                    &state,
                    StopReason::OutputBudgetExceeded,
                    response,
                ));
            }

            if state_exceeded_total_reasoning(&state, self.config.max_total_reasoning_tokens) {
                return Ok(build_result(
                    &state,
                    StopReason::ReasoningBudgetExceeded,
                    response,
                ));
            }

            if !response.has_tool_calls() {
                return Ok(build_result(&state, StopReason::Complete, response));
            }

            let tool_calls: Vec<_> = response.tool_calls().cloned().collect();
            state_push_assistant(&mut state, &mut ctx, response.content.clone());
            state_add_tool_calls(&mut state, tool_calls.len());

            let batch =
                execute_batch_with(&tool_calls, &round.snapshot, &self.executor.retry_policy())
                    .await;

            if batch.panicked {
                tracing::warn!("tool batch task panicked — error results filled in by executor");
            }

            state_push_tool_results(
                &mut state,
                &mut ctx,
                batch.results,
                &self.config.context_budget,
            );

            tracing::debug!(
                iteration = get_iterations(&state),
                tool_calls = tool_calls.len(),
                "tool-use loop iteration"
            );
        }
    }

    /// 流式执行，返回事件接收器
    pub fn execute_stream(&self, messages: Vec<Message>) -> AgentStream {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let model = self.model.clone();
        let executor = self.executor.clone();
        let config = self.config.clone();
        let deps = self.deps.clone();

        tokio::spawn(async move {
            let initial_messages = match build_request_messages_inner(&config, &messages) {
                Ok(m) => m,
                Err(e) => {
                    let _ = tokio::sync::mpsc::Sender::send(
                        &tx,
                        AgentEvent::LoopError {
                            error: e,
                            iterations: 0,
                        },
                    )
                    .await;
                    return;
                }
            };
            let mut state = create_initial_state(&initial_messages);
            let mut ctx = AgentExecutionContext::new(&initial_messages);
            let mut last_response: Option<ChatResponse> = None;
            let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());

            loop {
                if state_reached_max(&state, config.max_iterations) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: build_result(
                                &state,
                                StopReason::MaxIterationsReached,
                                last_response.unwrap_or_else(empty_response),
                            ),
                        },
                    )
                    .await;
                    return;
                }

                state_next_iteration(&mut state);
                if let Some(compact_result) =
                    state_compact(&mut state, &mut ctx, &config.context_budget, &*compactor)
                {
                    let _ = emit(
                        &tx,
                        AgentEvent::ContextCompacted {
                            before_tokens: compact_result.before_tokens,
                            after_tokens: compact_result.after_tokens,
                            removed_messages: compact_result.removed_messages,
                        },
                    )
                    .await;
                }

                let round = ResolvedRound::new(executor.snapshot().await);

                let req = super::config::build_request_inner_with_round(
                    &model,
                    &get_messages(&state),
                    config.max_output_tokens,
                    &config.request_options,
                    get_iterations(&state) as usize,
                    &round.definitions,
                );

                // 内联 fallback 重试循环
                let iteration = get_iterations(&state) as usize;
                let attempt_state = state.clone();
                let attempt_ctx = ctx.clone();
                let mut attempt: usize = 1;

                let result = loop {
                    let iter_result = do_stream_iteration(
                        model.clone(),
                        tx.clone(),
                        executor.clone(),
                        attempt_state.clone(),
                        attempt_ctx.clone(),
                        req.clone(),
                        config.context_budget.clone(),
                        config.max_output_tokens,
                        config.stream_thinking,
                        round.clone(),
                    )
                    .await;

                    match iter_result.result {
                        Ok((r, s)) => break Ok((r, s, iter_result.ctx)),
                        Err(ref err) => {
                            tracing::warn!(
                                attempt = attempt,
                                error = %err,
                                stream_started = iter_result.stream_started,
                                "stream iteration failed, fallback handling"
                            );

                            if iter_result.stream_started {
                                let e: LlmError = err.clone();
                                break Err(e);
                            }

                            let messages = get_messages(&attempt_state);
                            let fallback_ctx = FallbackContext {
                                error: err,
                                attempt,
                                iterations: iteration,
                                conversation: std::sync::Arc::from(messages.as_slice()),
                            };
                            match deps.fallback.handle(&fallback_ctx).await {
                                FallbackAction::Retry => {
                                    attempt += 1;
                                }
                                FallbackAction::Abort => {
                                    break Err(err.clone());
                                }
                            }
                        }
                    }
                };

                // 成功时合并 state
                let result = match result {
                    Ok((r, s, updated_ctx)) => {
                        state = s;
                        ctx = updated_ctx;
                        Ok(r)
                    }
                    Err(e) => Err(e),
                };

                // 总预算检查
                if state_exceeded_total_output(&state, config.max_total_output_tokens) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: build_result(
                                &state,
                                StopReason::OutputBudgetExceeded,
                                last_response.unwrap_or_else(empty_response),
                            ),
                        },
                    )
                    .await;
                    return;
                }

                if state_exceeded_total_reasoning(&state, config.max_total_reasoning_tokens) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: build_result(
                                &state,
                                StopReason::ReasoningBudgetExceeded,
                                last_response.unwrap_or_else(empty_response),
                            ),
                        },
                    )
                    .await;
                    return;
                }

                match result {
                    Ok(StreamIterResult::Continue { response }) => {
                        last_response = Some(response);
                    }
                    Ok(StreamIterResult::Complete { response }) => {
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: build_result(&state, StopReason::Complete, response),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::Cancelled { response }) => {
                        let resp = response
                            .or(last_response.take())
                            .unwrap_or_else(empty_response);
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: build_result(&state, StopReason::Cancelled, resp),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::OutputBudgetExceeded { response }) => {
                        tracing::warn!(
                            total_output_tokens = get_output_tokens(&state),
                            "single-round output budget exceeded, stopping agent"
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: build_result(
                                    &state,
                                    StopReason::OutputBudgetExceeded,
                                    response,
                                ),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::ReasoningBudgetExceeded { response }) => {
                        tracing::warn!(
                            total_reasoning_tokens = get_reasoning_tokens(&state),
                            "single-round reasoning budget exceeded, stopping agent"
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: build_result(
                                    &state,
                                    StopReason::ReasoningBudgetExceeded,
                                    response,
                                ),
                            },
                        )
                        .await;
                        return;
                    }
                    Err(e) => {
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopError {
                                error: e,
                                iterations: get_iterations(&state) as usize,
                            },
                        )
                        .await;
                        return;
                    }
                }
            }
        });

        rx
    }
}
