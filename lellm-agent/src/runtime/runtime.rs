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
use lellm_graph::State;
use lellm_provider::ResolvedModel;
use std::sync::Arc;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_messages_inner, empty_response};
use super::context::{
    ContextCompactor, LocalCompactor, estimate_reasoning_block, estimate_text, estimate_tokens,
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

    /// 非流式执行。
    ///
    /// 从 `state` 中读取消息（通过 `message_key`），执行 Agent Loop，
    /// 将所有状态写入 `state`。
    pub async fn execute(
        &self,
        state: &mut State,
        message_key: &str,
    ) -> Result<ToolUseResult, LlmError> {
        let initial_messages = build_request_messages_inner(
            &self.config,
            &super::state::get_messages(state, message_key),
        )?;
        // 初始化 State 中的 Agent 状态
        super::state::set_messages(state, message_key, &initial_messages);
        super::state::set_usize(
            state,
            &super::state::agent_key("agent", "estimated_tokens"),
            super::context::estimate_tokens(&initial_messages),
        );
        super::state::set_usize(
            state,
            &super::state::agent_key("agent", super::state::SK_ITERATIONS),
            0,
        );
        super::state::set_usize(
            state,
            &super::state::agent_key("agent", super::state::SK_TOOL_CALLS),
            0,
        );
        super::state::set_usize(
            state,
            &super::state::agent_key("agent", super::state::SK_OUTPUT_TOKENS),
            0,
        );
        super::state::set_usize(
            state,
            &super::state::agent_key("agent", super::state::SK_REASONING_TOKENS),
            0,
        );
        let mut last_response: Option<ChatResponse> = None;
        let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());
        let agent_prefix = "agent";

        loop {
            // 检查最大迭代
            let iterations: usize = super::state::get_usize(
                state,
                &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
            );
            if iterations >= self.config.max_iterations {
                return Ok(super::state::build_result_from_state(
                    state,
                    agent_prefix,
                    message_key,
                    StopReason::MaxIterationsReached,
                    last_response.unwrap_or_else(empty_response),
                ));
            }

            // 进入下一轮
            super::state::set_usize(
                state,
                &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
                iterations + 1,
            );

            // 压缩
            {
                let estimated: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, "estimated_tokens"),
                );
                if self.config.context_budget.should_compact(estimated) {
                    let msgs = super::state::get_messages(state, message_key);
                    let result = compactor.compact(&msgs, &self.config.context_budget);
                    super::state::set_messages(state, message_key, &result.messages);
                    super::state::set_usize(
                        state,
                        &super::state::agent_key(agent_prefix, "estimated_tokens"),
                        result.after_tokens,
                    );
                }
            }

            // 1.5 解析本轮工具快照（每轮一次，固定工具集）
            let round = ResolvedRound::new(self.executor.snapshot().await);

            // 2. 构建请求
            let messages_for_req = super::state::get_messages(state, message_key);
            let iterations_now: usize = super::state::get_usize(
                state,
                &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
            );
            let req = super::config::build_request_inner_with_round(
                &self.model,
                &messages_for_req,
                self.config.max_output_tokens,
                &self.config.request_options,
                iterations_now,
                &round.definitions,
            );

            // 3. 执行 Provider
            let msg_snapshot = messages_for_req.clone();
            let response = execute_with_fallback(
                &self.deps.fallback,
                |_| true, // 非流式模式允许重试
                || self.model.provider.call(&req),
                iterations_now,
                &msg_snapshot,
            )
            .await?;
            last_response = Some(response.clone());

            // 4. 单轮推理预算检查（非流式路径）
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
                    return Ok(super::state::build_result_from_state(
                        state,
                        agent_prefix,
                        message_key,
                        StopReason::ReasoningBudgetExceeded,
                        response,
                    ));
                }
            }

            // 5. 后处理响应 — Token 统计
            {
                let mut output_add: usize = 0;
                let mut reasoning_add: usize = 0;
                for b in &response.content {
                    match b {
                        lellm_core::ContentBlock::Text(t) => output_add += estimate_text(&t.text),
                        lellm_core::ContentBlock::Thinking(th) => {
                            reasoning_add += estimate_reasoning_block(th)
                        }
                        lellm_core::ContentBlock::Image { .. }
                        | lellm_core::ContentBlock::ToolCall(_) => {}
                    }
                }
                let cur_output: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_OUTPUT_TOKENS),
                );
                super::state::set_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_OUTPUT_TOKENS),
                    cur_output + output_add,
                );
                let cur_reasoning: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_REASONING_TOKENS),
                );
                super::state::set_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_REASONING_TOKENS),
                    cur_reasoning + reasoning_add,
                );
            }

            // 预算检查
            {
                let total_output: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_OUTPUT_TOKENS),
                );
                if let Some(limit) = self.config.max_total_output_tokens {
                    if total_output >= limit as usize {
                        super::state::set_stop_reason(
                            state,
                            &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                            &StopReason::OutputBudgetExceeded,
                        );
                        return Ok(super::state::build_result_from_state(
                            state,
                            agent_prefix,
                            message_key,
                            StopReason::OutputBudgetExceeded,
                            response,
                        ));
                    }
                }
            }

            {
                let total_reasoning: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_REASONING_TOKENS),
                );
                if let Some(limit) = self.config.max_total_reasoning_tokens {
                    if total_reasoning >= limit as usize {
                        super::state::set_stop_reason(
                            state,
                            &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                            &StopReason::ReasoningBudgetExceeded,
                        );
                        return Ok(super::state::build_result_from_state(
                            state,
                            agent_prefix,
                            message_key,
                            StopReason::ReasoningBudgetExceeded,
                            response,
                        ));
                    }
                }
            }

            if !response.has_tool_calls() {
                super::state::set_stop_reason(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                    &StopReason::Complete,
                );
                return Ok(super::state::build_result_from_state(
                    state,
                    agent_prefix,
                    message_key,
                    StopReason::Complete,
                    response,
                ));
            }

            // 有 tool_calls — 追加 assistant 消息
            {
                let msg = lellm_core::Message::Assistant {
                    content: response.content.clone(),
                };
                let tokens = estimate_tokens(&[msg.clone()]);
                let cur_est: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, "estimated_tokens"),
                );
                super::state::set_usize(
                    state,
                    &super::state::agent_key(agent_prefix, "estimated_tokens"),
                    cur_est + tokens,
                );
                let mut msgs = super::state::get_messages(state, message_key);
                msgs.push(msg);
                super::state::set_messages(state, message_key, &msgs);
            }

            let tool_calls: Vec<_> = response.tool_calls().cloned().collect();
            {
                let cur: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_TOOL_CALLS),
                );
                super::state::set_usize(
                    state,
                    &super::state::agent_key(agent_prefix, super::state::SK_TOOL_CALLS),
                    cur + tool_calls.len(),
                );
            }

            let batch =
                execute_batch_with(&tool_calls, &round.snapshot, &self.executor.retry_policy())
                    .await;

            if batch.panicked {
                tracing::warn!("tool batch task panicked — error results filled in by executor");
            }

            // 截断工具结果并追加
            let truncated_results: Vec<Message> = batch
                .results
                .into_iter()
                .map(|m| {
                    if let Message::ToolResult {
                        ref tool_call_id,
                        is_error: false,
                        ref content,
                    } = m
                    {
                        let truncated = self
                            .config
                            .context_budget
                            .truncate_tool_result_blocks(content);
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
            {
                let tokens = estimate_tokens(&truncated_results);
                let cur_est: usize = super::state::get_usize(
                    state,
                    &super::state::agent_key(agent_prefix, "estimated_tokens"),
                );
                super::state::set_usize(
                    state,
                    &super::state::agent_key(agent_prefix, "estimated_tokens"),
                    cur_est + tokens,
                );
                let mut msgs = super::state::get_messages(state, message_key);
                msgs.extend(truncated_results);
                super::state::set_messages(state, message_key, &msgs);
            }

            tracing::debug!(
                iteration = iterations_now,
                tool_calls = tool_calls.len(),
                "tool-use loop iteration"
            );
        }
    }

    /// 流式执行，返回事件接收器。
    ///
    /// 克隆 `state` 在后台任务中工作。最终结果通过 `AgentEvent::LoopEnd`
    /// 携带 `ToolUseResult` 返回。
    pub fn execute_stream(&self, state: State, message_key: &str) -> AgentStream {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let model = self.model.clone();
        let executor = self.executor.clone();
        let config = self.config.clone();
        let deps = self.deps.clone();
        let message_key = message_key.to_string();
        let agent_prefix = "agent";

        tokio::spawn(async move {
            // 初始化消息
            let mut state = state;
            let initial_messages = match build_request_messages_inner(
                &config,
                &super::state::get_messages(&state, &message_key),
            ) {
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

            // 初始化 State 中的 Agent 状态
            super::state::set_messages(&mut state, &message_key, &initial_messages);
            super::state::set_usize(
                &mut state,
                &super::state::agent_key(agent_prefix, "estimated_tokens"),
                super::context::estimate_tokens(&initial_messages),
            );
            super::state::set_usize(
                &mut state,
                &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
                0,
            );
            super::state::set_usize(
                &mut state,
                &super::state::agent_key(agent_prefix, super::state::SK_TOOL_CALLS),
                0,
            );
            super::state::set_usize(
                &mut state,
                &super::state::agent_key(agent_prefix, super::state::SK_OUTPUT_TOKENS),
                0,
            );
            super::state::set_usize(
                &mut state,
                &super::state::agent_key(agent_prefix, super::state::SK_REASONING_TOKENS),
                0,
            );

            let mut last_response: Option<ChatResponse> = None;
            let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());

            loop {
                // 检查最大迭代
                let iterations: usize = super::state::get_usize(
                    &state,
                    &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
                );
                if iterations >= config.max_iterations {
                    super::state::set_stop_reason(
                        &mut state,
                        &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                        &StopReason::MaxIterationsReached,
                    );
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: super::state::build_result_from_state(
                                &state,
                                agent_prefix,
                                &message_key,
                                StopReason::MaxIterationsReached,
                                last_response.unwrap_or_else(empty_response),
                            ),
                        },
                    )
                    .await;
                    return;
                }

                // 进入下一轮
                super::state::set_usize(
                    &mut state,
                    &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
                    iterations + 1,
                );

                // 压缩
                {
                    let estimated: usize = super::state::get_usize(
                        &state,
                        &super::state::agent_key(agent_prefix, "estimated_tokens"),
                    );
                    if config.context_budget.should_compact(estimated) {
                        let msgs = super::state::get_messages(&state, &message_key);
                        let before_tokens = estimated;
                        let result = compactor.compact(&msgs, &config.context_budget);
                        super::state::set_messages(&mut state, &message_key, &result.messages);
                        super::state::set_usize(
                            &mut state,
                            &super::state::agent_key(agent_prefix, "estimated_tokens"),
                            result.after_tokens,
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::ContextCompacted {
                                before_tokens,
                                after_tokens: result.after_tokens,
                                removed_messages: result.removed_messages,
                            },
                        )
                        .await;
                    }
                }

                // 解析本轮工具快照（每轮一次，固定工具集）
                let round = ResolvedRound::new(executor.snapshot().await);

                let messages_for_req = super::state::get_messages(&state, &message_key);
                let iterations_now: usize = super::state::get_usize(
                    &state,
                    &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
                );
                let req = super::config::build_request_inner_with_round(
                    &model,
                    &messages_for_req,
                    config.max_output_tokens,
                    &config.request_options,
                    iterations_now,
                    &round.definitions,
                );

                // 内联 fallback 重试循环
                let mut attempt_messages = messages_for_req.clone();
                let mut attempt: usize = 1;

                // 流式迭代 — 直接在 State 上操作
                let mut result: Option<Result<StreamIterResult, LlmError>> = None; // initialized before each break
                loop {
                    let iter_result = do_stream_iteration(
                        model.clone(),
                        tx.clone(),
                        executor.clone(),
                        &mut state,
                        &message_key,
                        req.clone(),
                        config.context_budget.clone(),
                        config.max_output_tokens,
                        config.stream_thinking,
                        round.clone(),
                    )
                    .await;

                    match iter_result.result {
                        Ok(v) => {
                            result = Some(Ok(v));
                            break;
                        }
                        Err(err) => {
                            tracing::warn!(
                                attempt = attempt,
                                error = %err,
                                stream_started = iter_result.stream_started,
                                "stream iteration failed, fallback handling"
                            );

                            if iter_result.stream_started {
                                result = Some(Err(err));
                                break;
                            }

                            let ctx = FallbackContext {
                                error: &err,
                                attempt,
                                iterations: iterations_now,
                                conversation: std::sync::Arc::from(attempt_messages.as_slice()),
                            };
                            match deps.fallback.handle(&ctx).await {
                                FallbackAction::Retry => {
                                    attempt += 1;
                                    attempt_messages =
                                        super::state::get_messages(&state, &message_key);
                                }
                                FallbackAction::Abort => {
                                    result = Some(Err(err));
                                    break;
                                }
                            }
                        }
                    }
                }
                let result = result.unwrap();

                // 总预算检查
                {
                    let total_output: usize = super::state::get_usize(
                        &state,
                        &super::state::agent_key(agent_prefix, super::state::SK_OUTPUT_TOKENS),
                    );
                    if let Some(limit) = config.max_total_output_tokens {
                        if total_output >= limit as usize {
                            super::state::set_stop_reason(
                                &mut state,
                                &super::state::agent_key(
                                    agent_prefix,
                                    super::state::SK_STOP_REASON,
                                ),
                                &StopReason::OutputBudgetExceeded,
                            );
                            let _ = emit(
                                &tx,
                                AgentEvent::LoopEnd {
                                    result: super::state::build_result_from_state(
                                        &state,
                                        agent_prefix,
                                        &message_key,
                                        StopReason::OutputBudgetExceeded,
                                        last_response.unwrap_or_else(empty_response),
                                    ),
                                },
                            )
                            .await;
                            return;
                        }
                    }

                    let total_reasoning: usize = super::state::get_usize(
                        &state,
                        &super::state::agent_key(agent_prefix, super::state::SK_REASONING_TOKENS),
                    );
                    if let Some(limit) = config.max_total_reasoning_tokens {
                        if total_reasoning >= limit as usize {
                            super::state::set_stop_reason(
                                &mut state,
                                &super::state::agent_key(
                                    agent_prefix,
                                    super::state::SK_STOP_REASON,
                                ),
                                &StopReason::ReasoningBudgetExceeded,
                            );
                            let _ = emit(
                                &tx,
                                AgentEvent::LoopEnd {
                                    result: super::state::build_result_from_state(
                                        &state,
                                        agent_prefix,
                                        &message_key,
                                        StopReason::ReasoningBudgetExceeded,
                                        last_response.unwrap_or_else(empty_response),
                                    ),
                                },
                            )
                            .await;
                            return;
                        }
                    }
                }

                match result {
                    Ok(StreamIterResult::Continue { response }) => {
                        last_response = Some(response.clone());
                    }
                    Ok(StreamIterResult::Complete { response }) => {
                        super::state::set_stop_reason(
                            &mut state,
                            &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                            &StopReason::Complete,
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: super::state::build_result_from_state(
                                    &state,
                                    agent_prefix,
                                    &message_key,
                                    StopReason::Complete,
                                    response.clone(),
                                ),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::Cancelled { response }) => {
                        let resp = response
                            .clone()
                            .or(last_response.take())
                            .unwrap_or_else(empty_response);
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: super::state::build_result_from_state(
                                    &state,
                                    agent_prefix,
                                    &message_key,
                                    StopReason::Cancelled,
                                    resp,
                                ),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::OutputBudgetExceeded { response }) => {
                        super::state::set_stop_reason(
                            &mut state,
                            &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                            &StopReason::OutputBudgetExceeded,
                        );
                        tracing::warn!(
                            total_output_tokens = super::state::get_usize(
                                &state,
                                &super::state::agent_key(
                                    agent_prefix,
                                    super::state::SK_OUTPUT_TOKENS
                                )
                            ),
                            "single-round output budget exceeded, stopping agent"
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: super::state::build_result_from_state(
                                    &state,
                                    agent_prefix,
                                    &message_key,
                                    StopReason::OutputBudgetExceeded,
                                    response.clone(),
                                ),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::ReasoningBudgetExceeded { response }) => {
                        super::state::set_stop_reason(
                            &mut state,
                            &super::state::agent_key(agent_prefix, super::state::SK_STOP_REASON),
                            &StopReason::ReasoningBudgetExceeded,
                        );
                        tracing::warn!(
                            total_reasoning_tokens = super::state::get_usize(
                                &state,
                                &super::state::agent_key(
                                    agent_prefix,
                                    super::state::SK_REASONING_TOKENS
                                )
                            ),
                            "single-round reasoning budget exceeded, stopping agent"
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: super::state::build_result_from_state(
                                    &state,
                                    agent_prefix,
                                    &message_key,
                                    StopReason::ReasoningBudgetExceeded,
                                    response.clone(),
                                ),
                            },
                        )
                        .await;
                        return;
                    }
                    Err(e) => {
                        let iterations: usize = super::state::get_usize(
                            &state,
                            &super::state::agent_key(agent_prefix, super::state::SK_ITERATIONS),
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopError {
                                error: e,
                                iterations,
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
