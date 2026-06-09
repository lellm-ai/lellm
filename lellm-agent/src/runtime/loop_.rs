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
use tokio::sync::mpsc::Sender;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_messages_inner, empty_response};
use super::context::{ContextCompactor, LocalCompactor, estimate_text, estimate_tokens};
use super::event::{AgentEvent, AgentStream, StopReason};
use super::iteration::{
    build_partial_response, emit, emit_and_execute_tools, execute_with_fallback,
};
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

    /// 追加工具执行结果到历史
    pub fn push_tool_results(&mut self, results: Vec<Message>) {
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
        budget: &super::context::ContextBudget,
        compactor: &dyn ContextCompactor,
    ) -> Option<super::context::CompactionResult> {
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

            // 4. 后处理响应（借用 state）
            state.add_output_from_content(&response.content);

            if state.exceeded_total_output(self.config.max_total_output_tokens) {
                return Ok(state.finish_output_budget(response));
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
            state.push_tool_results(results);

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

                // 把 stream() + process_stream_iteration() 整体包进 fallback，
                // 这样流消费失败（SSE 中途断掉）也能触发重试。
                let model_for_fallback = model.clone();
                let tx_for_fallback = tx.clone();
                let executor_for_fallback = executor.clone();
                let budget_for_fallback = config.context_budget.clone();
                let max_output_tokens_for_fallback = config.max_output_tokens;
                let req_for_fallback = req.clone();
                let state_for_clone = state.clone();
                let iteration_count = state.iterations;
                let state_messages = state.messages.clone();

                let result: Result<(StreamIterResult, LoopState), LlmError> =
                    execute_with_fallback(
                        &deps.fallback,
                        move || {
                            let model = model_for_fallback.clone();
                            let tx = tx_for_fallback.clone();
                            let executor = executor_for_fallback.clone();
                            let budget = budget_for_fallback.clone();
                            let max_output_tokens = max_output_tokens_for_fallback;
                            let req = req_for_fallback.clone();
                            let max_reasoning_tokens = req.max_reasoning_tokens;
                            let mut attempt_state = state_for_clone.clone();

                            async move {
                                let mut stream = model.provider.stream(&req).await?;
                                let mut text_buffer = String::new();
                                let mut thinking_buffer = String::new();
                                let mut redacted_buffer: Option<String> = None;

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
                                )
                                .await?;

                                Ok((iter_result, attempt_state))
                            }
                        },
                        iteration_count,
                        &state_messages,
                    )
                    .await;

                // 成功时合并 state
                let result = match result {
                    Ok((r, s)) => {
                        state = s;
                        Ok(r)
                    }
                    Err(e) => Err(e),
                };

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

// ─── 流式单轮迭代 ────────────────────────────────────────────────

/// 流式单轮迭代的合法结果 — 枚举保证类型安全。
///
/// **设计原则：** 仅表达"一次迭代成功完成后的状态"。
/// 错误通过 `Result<StreamIterResult, LlmError>` 的 `Err` 表达。
enum StreamIterResult {
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

/// 处理流式单轮迭代。
///
/// 返回 `Ok(StreamIterResult)` 表示迭代完成，`Err(LlmError)` 表示 Provider 错误。
///
/// `max_output_tokens` — 单轮输出的 Token 上限（保险丝），在流式消费时实时检查。
/// `max_reasoning_tokens` — 单轮推理的 Token 上限（保险丝），可选。
async fn process_stream_iteration(
    tx: &Sender<AgentEvent>,
    executor: &ToolExecutor,
    state: &mut LoopState,
    stream: &mut lellm_provider::ProviderStream,
    text_buffer: &mut String,
    thinking_buffer: &mut String,
    redacted_buffer: &mut Option<String>,
    budget: &super::context::ContextBudget,
    max_output_tokens: u32,
    max_reasoning_tokens: Option<u32>,
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
                round_reasoning_tokens += estimate_text(thinking);
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

        if !emit(tx, AgentEvent::Provider(ev.clone())).await {
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
                state.add_tool_calls(pending_tool_calls.len());

                let results =
                    emit_and_execute_tools(tx, executor, &pending_tool_calls, budget).await;
                if results.is_none() {
                    return Ok(StreamIterResult::Cancelled {
                        response: Some(response),
                    });
                }
                state.push_tool_results(results.unwrap());

                tracing::debug!(
                    iteration = state.iterations,
                    tool_calls = pending_tool_calls.len(),
                    "tool-use stream iteration"
                );

                return Ok(StreamIterResult::Continue { response });
            } else {
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
