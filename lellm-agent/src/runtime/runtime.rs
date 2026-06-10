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
//! ├── executor:    ToolExecutor      (工具注册表 + 执行引擎)
//! ├── config:      ToolUseConfig     (纯参数, Clone + Send + Sync)
//! └── deps:        ToolUseDeps       (策略服务, Arc 包裹)
//! ```

use lellm_core::{ChatResponse, LlmError, Message};
use lellm_provider::ResolvedModel;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_messages_inner, empty_response};
use super::context::{
    CompactionResult, ContextBudget, ContextCompactor, LocalCompactor, estimate_text,
    estimate_tokens,
};
use super::event::{AgentEvent, AgentStream, StopReason};
use super::fallback::{FallbackAction, FallbackContext};
use super::iteration::{StreamIterResult, do_stream_iteration, emit, execute_with_fallback};
use super::tools::ToolExecutor;

// ─── 循环状态机 ───────────────────────────────────────────────

/// Agent Loop 共享状态 — 非流式与流式执行模式共用。
///
/// `Clone` 用于迭代重试时回滚状态（snapshot → restore）。
#[derive(Debug, Clone)]
pub struct LoopState {
    pub messages: Vec<Message>,
    /// 消息历史的估算 Token 数（与 messages 同步更新）
    pub estimated_tokens: usize,
    pub iterations: usize,
    pub tool_calls_executed: usize,
    /// 整个 Agent Run 的累计输出 Token 数（Text，不含 Thinking 和 Tool Call）
    /// 用于 max_total_output_tokens 预算检查。
    pub total_output_tokens: usize,
    /// 整个 Agent Run 的累计推理 Token 数（Thinking，不含 Text 和 Tool Call）
    /// 用于可观测性；单轮推理预算通过 `ChatRequest.max_reasoning_tokens` 透传给 Provider。
    pub total_reasoning_tokens: usize,
}

impl LoopState {
    pub fn new(messages: Vec<Message>) -> Self {
        let estimated_tokens = estimate_tokens(&messages);
        Self {
            messages,
            estimated_tokens,
            iterations: 0,
            tool_calls_executed: 0,
            total_output_tokens: 0,
            total_reasoning_tokens: 0,
        }
    }

    /// 累计输出 Token（Text）
    pub fn add_output_tokens(&mut self, tokens: usize) {
        self.total_output_tokens += tokens;
    }

    /// 累计推理 Token（Thinking）
    pub fn add_reasoning_tokens(&mut self, tokens: usize) {
        self.total_reasoning_tokens += tokens;
    }

    /// 检查是否超过总输出预算
    pub fn exceeded_total_output(&self, max: Option<u32>) -> bool {
        match max {
            Some(limit) => self.total_output_tokens >= limit as usize,
            None => false,
        }
    }

    /// 检查是否超过总推理预算
    pub fn exceeded_total_reasoning(&self, max: Option<u32>) -> bool {
        match max {
            Some(limit) => self.total_reasoning_tokens >= limit as usize,
            None => false,
        }
    }

    /// 从 ContentBlock 分离估算 Output（Text）和 Reasoning（Thinking）Token
    pub fn add_output_from_content(&mut self, content: &[lellm_core::ContentBlock]) {
        let mut output_tokens: usize = 0;
        let mut reasoning_tokens: usize = 0;
        for b in content {
            match b {
                lellm_core::ContentBlock::Text(t) => output_tokens += estimate_text(&t.text),
                lellm_core::ContentBlock::Thinking(th) => {
                    reasoning_tokens += estimate_text(&th.thinking)
                }
                lellm_core::ContentBlock::Image { .. } | lellm_core::ContentBlock::ToolCall(_) => {}
            }
        }
        self.total_output_tokens += output_tokens;
        self.total_reasoning_tokens += reasoning_tokens;
    }

    /// 追加 Assistant 响应到历史
    pub fn push_assistant(&mut self, content: Vec<lellm_core::ContentBlock>) {
        let msg = Message::Assistant {
            content: content.clone(),
        };
        let tokens = estimate_tokens(&[msg]);
        self.estimated_tokens += tokens;
        self.messages.push(Message::Assistant { content });
    }

    /// 追加工具执行结果到历史。
    ///
    /// 在注入前对成功结果执行截断（`max_tool_result_chars`），
    /// 失败结果不截断（错误信息应完整保留）。
    pub fn push_tool_results(
        &mut self,
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
        self.estimated_tokens += tokens;
        self.messages.extend(results);
    }

    /// 记录本轮工具调用数量
    pub fn add_tool_calls(&mut self, count: usize) {
        self.tool_calls_executed += count;
    }

    /// 进入下一轮迭代
    pub fn next_iteration(&mut self) {
        self.iterations += 1;
    }

    /// 判断是否已达到最大轮次
    pub fn reached_max(&self, max_iterations: usize) -> bool {
        self.iterations >= max_iterations
    }

    /// 对消息历史执行压缩。
    /// 返回压缩结果（用于发射事件），`None` 表示无需压缩。
    pub fn compact(
        &mut self,
        budget: &ContextBudget,
        compactor: &dyn ContextCompactor,
    ) -> Option<CompactionResult> {
        if !budget.should_compact(self.estimated_tokens) {
            return None;
        }
        let result = compactor.compact(&self.messages, budget);
        self.messages = result.messages.clone();
        self.estimated_tokens = result.after_tokens;
        Some(result)
    }

    /// 构建最终执行结果
    pub fn finish(&self, stop_reason: StopReason, response: ChatResponse) -> ToolUseResult {
        ToolUseResult {
            stop_reason,
            response,
            messages: self.messages.clone(),
            iterations: self.iterations,
            tool_calls_executed: self.tool_calls_executed,
        }
    }

    /// 构建正常完成结果
    pub fn finish_complete(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::Complete, response)
    }

    /// 构建达到最大轮次结果
    pub fn finish_max_iterations(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::MaxIterationsReached, response)
    }

    /// 构建外部取消结果
    pub fn finish_cancelled(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::Cancelled, response)
    }

    /// 构建输出预算超限结果
    pub fn finish_output_budget(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::OutputBudgetExceeded, response)
    }

    /// 构建推理预算超限结果
    pub fn finish_reasoning_budget(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::ReasoningBudgetExceeded, response)
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
    /// 执行过程中调用的工具总次数
    pub tool_calls_executed: usize,
}

impl ToolUseResult {
    /// 仅 `StopReason::Complete` 返回 `true`。
    pub fn is_success(&self) -> bool {
        matches!(self.stop_reason, StopReason::Complete)
    }
}

// ─── ToolUseLoop ────────────────────────────────────────────────

/// 管理 LLM 与工具调用闭环。
///
/// 内部全为 Arc/Clone，clone 为 O(1)，支持并发 execute。
///
/// # 构造
///
/// **推荐（Builder API）：**
/// ```rust,ignore
/// let agent = AgentBuilder::new(model)
///     .system_prompt("你是助手".into())
///     .tool(search_tool)
///     .max_iterations(20)
///     .build();
/// ```
///
/// **高级（直接构造）：**
/// ```rust,ignore
/// let agent = ToolUseLoop::new(model, executor, config, deps);
/// ```
#[derive(Clone)]
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    config: ToolUseConfig,
    deps: ToolUseDeps,
}

impl ToolUseLoop {
    /// 构造 ToolUseLoop。
    ///
    /// `config` 为纯参数，`deps` 为策略服务。
    pub fn new(
        model: ResolvedModel,
        executor: ToolExecutor,
        config: ToolUseConfig,
        deps: ToolUseDeps,
    ) -> Self {
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
    ///
    /// 语义：
    /// - `Ok(ToolUseResult)` — Agent 层完成（含 MaxIterationsReached）
    /// - `Err(LlmError)` — Provider 调用失败
    ///
    /// `&self` 借用，不消费 self — 支持复用同一个 agent 做多次对话。
    pub async fn execute(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let initial_messages = build_request_messages_inner(&self.config, &messages)?;
        let mut state = LoopState::new(initial_messages);
        let mut last_response: Option<ChatResponse> = None;
        let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());

        loop {
            // 1. 检查终止 + 推进迭代（借用 state）
            if state.reached_max(self.config.max_iterations) {
                return Ok(
                    state.finish_max_iterations(last_response.unwrap_or_else(empty_response))
                );
            }
            state.next_iteration();
            state.compact(&self.config.context_budget, &*compactor);

            // 2. 构建请求（借用 state）
            let req = super::config::build_request_inner_with_round(
                &self.model,
                &self.executor,
                &state.messages,
                self.config.max_output_tokens,
                &self.config.request_options,
                state.iterations,
            );

            // 3. 执行 Provider（需要 state 快照）
            let iteration = state.iterations;
            let msg_snapshot = state.messages.clone();
            let response = execute_with_fallback(
                &self.deps.fallback,
                || self.model.provider.call(&req),
                iteration,
                &msg_snapshot,
            )
            .await?;
            last_response = Some(response.clone());

            // 4. 单轮推理预算检查（非流式路径）
            if let Some(limit) = self.config.request_options.max_reasoning_tokens {
                let round_reasoning: usize = response
                    .content
                    .iter()
                    .filter_map(|b| {
                        if let lellm_core::ContentBlock::Thinking(th) = b {
                            Some(estimate_text(&th.thinking))
                        } else {
                            None
                        }
                    })
                    .sum();
                if round_reasoning > limit as usize {
                    tracing::warn!(
                        round_reasoning,
                        max_reasoning_tokens = limit,
                        "single-round reasoning budget exceeded (non-stream)"
                    );
                    return Ok(state.finish_reasoning_budget(response));
                }
            }

            // 5. 后处理响应（借用 state）
            state.add_output_from_content(&response.content);

            if state.exceeded_total_output(self.config.max_total_output_tokens) {
                return Ok(state.finish_output_budget(response));
            }

            if state.exceeded_total_reasoning(self.config.max_total_reasoning_tokens) {
                return Ok(state.finish_reasoning_budget(response));
            }

            if !response.has_tool_calls() {
                return Ok(state.finish_complete(response));
            }

            let tool_calls: Vec<_> = response.tool_calls().cloned().collect();
            state.push_assistant(response.content.clone());
            state.add_tool_calls(tool_calls.len());

            let batch = self.executor.execute_batch(&tool_calls).await;

            let results = if let Some(e) = &batch.panicked {
                tracing::error!(error = %e, "tool batch task panicked");
                let mut results = batch.completed;
                let completed_ids: std::collections::HashSet<String> = results
                    .iter()
                    .filter_map(|m| {
                        if let Message::ToolResult { tool_call_id, .. } = m {
                            Some(tool_call_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                for tc in &tool_calls {
                    if !completed_ids.contains(&tc.id) {
                        results.push(Message::ToolResult {
                            tool_call_id: tc.id.clone(),
                            is_error: true,
                            content: lellm_core::text_block(format!("tool task failed: {e}")),
                        });
                    }
                }
                results
            } else {
                batch.completed
            };

            // 工具结果截断统一在 push_tool_results() 中执行
            state.push_tool_results(results, &self.config.context_budget);

            tracing::debug!(
                iteration = state.iterations,
                tool_calls = tool_calls.len(),
                "tool-use loop iteration"
            );
        }
    }

    /// 流式执行，返回事件接收器
    ///
    /// **Agent Stream Contract：**
    /// - 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
    /// - 业务失败：`LoopError` 恰好一次，然后 channel 关闭
    /// - 运行时异常：channel 直接关闭（未收到终态事件）
    ///
    /// `&self` 借用，不消费 self — 支持复用同一个 agent。
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
            let mut state = LoopState::new(initial_messages);
            let mut last_response: Option<ChatResponse> = None;
            let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());

            loop {
                if state.reached_max(config.max_iterations) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: state.finish_max_iterations(
                                last_response.unwrap_or_else(empty_response),
                            ),
                        },
                    )
                    .await;
                    return;
                }

                state.next_iteration();
                if let Some(compact_result) = state.compact(&config.context_budget, &*compactor) {
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

                let req = super::config::build_request_inner_with_round(
                    &model,
                    &executor,
                    &state.messages,
                    config.max_output_tokens,
                    &config.request_options,
                    state.iterations,
                );

                // 内联 fallback 重试循环 — 不用闭包捕获，消除 _for_fallback 变量爆炸。
                let iteration = state.iterations;
                let attempt_state = state.clone();
                let mut attempt: usize = 1;

                let result = loop {
                    let iter_result = do_stream_iteration(
                        model.clone(),
                        tx.clone(),
                        executor.clone(),
                        attempt_state.clone(),
                        req.clone(),
                        config.context_budget.clone(),
                        config.max_output_tokens,
                        config.stream_thinking,
                    )
                    .await;

                    match iter_result.result {
                        Ok(v) => break Ok(v),
                        Err(ref err) => {
                            tracing::warn!(
                                attempt = attempt,
                                error = %err,
                                stream_started = iter_result.stream_started,
                                "stream iteration failed, fallback handling"
                            );

                            // stream 已打开 → 事件可能已发出 → 禁止 Retry，直接 Abort
                            if iter_result.stream_started {
                                let e: LlmError = err.clone();
                                break Err(e);
                            }

                            let ctx = FallbackContext {
                                error: err,
                                attempt,
                                iterations: iteration,
                                conversation: std::sync::Arc::from(
                                    attempt_state.messages.as_slice(),
                                ),
                            };
                            match deps.fallback.handle(&ctx).await {
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
                    Ok((r, s)) => {
                        state = s;
                        Ok(r)
                    }
                    Err(e) => Err(e),
                };

                // 总预算检查（与非流式路径对齐）
                if state.exceeded_total_output(config.max_total_output_tokens) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: state.finish_output_budget(last_response.unwrap_or_else(empty_response)),
                        },
                    )
                    .await;
                    return;
                }

                if state.exceeded_total_reasoning(config.max_total_reasoning_tokens) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: state.finish_reasoning_budget(last_response.unwrap_or_else(empty_response)),
                        },
                    )
                    .await;
                    return;
                }

                match result {
                    Ok(StreamIterResult::Continue { response, .. }) => {
                        last_response = Some(response);
                    }
                    Ok(StreamIterResult::Complete { response }) => {
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: state.finish_complete(response),
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
                                result: state.finish_cancelled(resp),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::OutputBudgetExceeded { response }) => {
                        tracing::warn!(
                            total_output_tokens = state.total_output_tokens,
                            "single-round output budget exceeded, stopping agent"
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: state.finish_output_budget(response),
                            },
                        )
                        .await;
                        return;
                    }
                    Ok(StreamIterResult::ReasoningBudgetExceeded { response }) => {
                        tracing::warn!(
                            total_reasoning_tokens = state.total_reasoning_tokens,
                            "single-round reasoning budget exceeded, stopping agent"
                        );
                        let _ = emit(
                            &tx,
                            AgentEvent::LoopEnd {
                                result: state.finish_reasoning_budget(response),
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
                                iterations: state.iterations,
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
