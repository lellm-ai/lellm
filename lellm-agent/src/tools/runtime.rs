//! Agent Runtime — LLM ↔ 工具调用闭环。
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

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message};
use lellm_provider::ResolvedModel;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use super::context::{ContextBudget, ContextCompactor, LocalCompactor, estimate_tokens};
use super::executor::ToolExecutor;
use super::fallback::{DefaultFallback, FallbackAction, FallbackContext, FallbackStrategy};
use super::{AgentEvent, AgentStream, StopReason};

// ─── 配置（纯参数）──────────────────────────────────────────────

/// ToolUseLoop 纯参数配置。
///
/// - `Clone` + `Send` + `Sync` — 可安全跨线程共享
/// - 仅包含数据字段，不含行为逻辑
/// - 未来可扩展为 `Serialize` / `Deserialize`
#[derive(Debug, Clone)]
pub struct ToolUseConfig {
    /// 系统提示（运行时注入，不修改 messages）
    pub system_prompt: Option<String>,
    /// 最大迭代轮次（默认 10）
    pub max_iterations: usize,
    /// 每次 LLM 请求的最大输出 token 数（默认 16k）
    ///
    /// 控制单次 Provider 调用的响应长度上限，防止模型输出过长。
    /// 会自动注入到 `ChatRequest.max_tokens`。
    pub max_output_tokens: u32,
    /// 上下文预算管理（默认开启）
    ///
    /// **v0.1**: 默认 `ContextBudget::default()`（max_tokens = 128,000）
    /// **v0.2**: 从 `ResolvedModel.context_window` 自动推导（window * 0.8）
    ///
    /// 若要关闭限制，设置 `max_tokens = usize::MAX`。
    pub context_budget: ContextBudget,
}

impl Default for ToolUseConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_iterations: 10,
            max_output_tokens: 16_000,
            context_budget: ContextBudget::default(),
        }
    }
}

// ─── 依赖（策略服务）────────────────────────────────────────────

/// ToolUseLoop 策略依赖。
///
/// 包含有行为逻辑的服务对象（Arc 包裹），与纯参数 Config 分离。
#[derive(Clone)]
pub struct ToolUseDeps {
    /// Provider 降级策略
    pub fallback: Arc<dyn FallbackStrategy>,
}

impl Default for ToolUseDeps {
    fn default() -> Self {
        Self {
            fallback: Arc::new(DefaultFallback::default()),
        }
    }
}

// ─── 辅助函数 ───────────────────────────────────────────────────

/// 检查消息列表中是否已存在 System 消息。
fn has_system_message(messages: &[Message]) -> bool {
    messages.iter().any(|m| matches!(m, Message::System { .. }))
}

/// 构建有效的请求消息列表（用于 spawned task，无法使用 &self）
fn build_request_messages_inner(
    config: &ToolUseConfig,
    messages: &[Message],
) -> Result<Vec<Message>, LlmError> {
    if let Some(ref sp) = config.system_prompt {
        if has_system_message(messages) {
            return Err(LlmError::DuplicateSystemPrompt);
        }
        let mut result = vec![Message::System {
            content: lellm_core::text_block(sp.clone()),
        }];
        result.extend(messages.iter().cloned());
        Ok(result)
    } else {
        Ok(messages.to_vec())
    }
}

/// 构建 ChatRequest（用于 spawned task）
fn build_request_inner(
    model: &ResolvedModel,
    executor: &ToolExecutor,
    messages: &[Message],
    max_output_tokens: u32,
) -> ChatRequest {
    ChatRequest {
        model: model.model.clone(),
        messages: messages.to_vec(),
        tools: executor.has_tools().then(|| executor.definitions()),
        max_tokens: Some(max_output_tokens),
        ..Default::default()
    }
}

// ─── 循环状态机 ───

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
}

impl LoopState {
    pub fn new(messages: Vec<Message>) -> Self {
        let estimated_tokens = estimate_tokens(&messages);
        Self {
            messages,
            estimated_tokens,
            iterations: 0,
            tool_calls_executed: 0,
        }
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
        budget: &ContextBudget,
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
}

// ─── 执行结果 ───

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

// ─── 辅助函数 ───

/// 发送事件，消费者丢弃 Receiver 时返回 `false`。
async fn emit(tx: &Sender<AgentEvent>, event: AgentEvent) -> bool {
    tx.send(event).await.is_ok()
}

/// 带 Fallback 重试的通用操作执行器（自由函数，spawned task 可用）。
///
/// **职责划分：**
/// - `FallbackContext` = 观察窗口（借用 `&LlmError`）
/// - Retry Loop = 错误所有者（Abort 时直接返回 owned `err`）
///
/// 零成本抽象 — 泛型 `F: FnMut() -> Fut`，无 `Box<dyn Future>`。
async fn execute_with_fallback<T, F, Fut>(
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

/// 构建空的 ChatResponse（边界情况兜底）
fn empty_response() -> ChatResponse {
    ChatResponse::new(
        lellm_core::text_block(String::new()),
        lellm_core::TokenUsage::default(),
        serde_json::Value::Null,
    )
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

    // ─── 内部方法 ─────────────────────────────────────────────

    /// 构建有效的请求消息列表：system_prompt + conversation_messages。
    /// 双来源冲突时返回 DuplicateSystemPrompt 错误。
    fn build_request_messages(&self, messages: &[Message]) -> Result<Vec<Message>, LlmError> {
        build_request_messages_inner(&self.config, messages)
    }

    /// 构建 ChatRequest，自动注入工具 Schema 和 max_tokens。
    fn build_request(&self, messages: &[Message]) -> ChatRequest {
        build_request_inner(
            &self.model,
            &self.executor,
            messages,
            self.config.max_output_tokens,
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
        let initial_messages = self.build_request_messages(&messages)?;
        let mut state = LoopState::new(initial_messages);
        let mut last_response: Option<ChatResponse> = None;
        let compactor: Box<dyn ContextCompactor> = Box::new(LocalCompactor::new());

        loop {
            if state.reached_max(self.config.max_iterations) {
                return Ok(
                    state.finish_max_iterations(last_response.unwrap_or_else(empty_response))
                );
            }

            state.next_iteration();

            // 上下文压缩检查 — ToolResult 加入后、下一轮请求前
            state.compact(&self.config.context_budget, &*compactor);

            let req = self.build_request(&state.messages);
            let response = execute_with_fallback(
                &self.deps.fallback,
                || self.model.provider.call(&req),
                state.iterations,
                &state.messages,
            )
            .await?;
            last_response = Some(response);

            if !last_response.as_ref().unwrap().has_tool_calls() {
                return Ok(state.finish_complete(last_response.unwrap()));
            }

            let tool_calls: Vec<_> = last_response
                .as_ref()
                .unwrap()
                .tool_calls()
                .cloned()
                .collect();

            let response_content = last_response.as_ref().unwrap().content.clone();
            state.push_assistant(response_content);
            state.add_tool_calls(tool_calls.len());

            let batch = self.executor.execute_batch(&tool_calls).await;

            // 追加已成功的结果
            let results = if let Some(e) = &batch.panicked {
                // spawned task panic — 为未完成的 tool_call 生成错误结果
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
            // 构建初始消息
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

                // 上下文压缩检查
                if let Some(result) = state.compact(&config.context_budget, &*compactor) {
                    let _ = emit(
                        &tx,
                        AgentEvent::ContextCompacted {
                            before_tokens: result.before_tokens,
                            after_tokens: result.after_tokens,
                            removed_messages: result.removed_messages,
                        },
                    )
                    .await;
                }

                let req = build_request_inner(
                    &model,
                    &executor,
                    &state.messages,
                    config.max_output_tokens,
                );

                // 把 stream() + process_stream_iteration() 整体包进 fallback，
                // 这样流消费失败（SSE 中途断掉）也能触发重试。
                // 每次尝试克隆 state，成功后合并回主 state；重试时从干净状态开始。
                let model_for_fallback = model.clone();
                let tx_for_fallback = tx.clone();
                let executor_for_fallback = executor.clone();
                let budget_for_fallback = config.context_budget.clone();
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
                            let req = req_for_fallback.clone();
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
                                )
                                .await?;

                                Ok((iter_result, attempt_state))
                            }
                        },
                        iteration_count,
                        &state_messages,
                    )
                    .await;

                // 成功时合并 state（process_stream_iteration 只在 ResponseComplete 时修改）
                let result = match result {
                    Ok((r, s)) => {
                        state = s;
                        Ok(r)
                    }
                    Err(e) => Err(e),
                };

                match result {
                    Ok(StreamIterResult::Continue {
                        response,
                        tool_calls: _,
                    }) => {
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

/// 流式单轮迭代的合法结果 — 枚举保证类型安全。
///
/// **设计原则：** 仅表达"一次迭代成功完成后的状态"。
/// 错误通过 `Result<StreamIterResult, LlmError>` 的 `Err` 表达。
enum StreamIterResult {
    /// 继续循环（有 tool_calls，应进入下一轮）
    Continue {
        response: ChatResponse,
        #[allow(dead_code)]
        tool_calls: Vec<lellm_core::ToolCall>,
    },
    /// 正常完成（无 tool_calls，Agent 已获得最终答案）
    Complete { response: ChatResponse },
    /// 消费者断开（不再继续）
    Cancelled { response: Option<ChatResponse> },
}

/// 处理流式单轮迭代。
///
/// 返回 `Ok(StreamIterResult)` 表示迭代完成，`Err(LlmError)` 表示 Provider 错误。
async fn process_stream_iteration(
    tx: &Sender<AgentEvent>,
    executor: &ToolExecutor,
    state: &mut LoopState,
    stream: &mut lellm_provider::ProviderStream,
    text_buffer: &mut String,
    thinking_buffer: &mut String,
    redacted_buffer: &mut Option<String>,
    budget: &ContextBudget,
) -> Result<StreamIterResult, LlmError> {
    use futures_util::StreamExt;

    while let Some(result) = stream.next().await {
        let ev = match result {
            Ok(ev) => ev,
            Err(e) => {
                return Err(e);
            }
        };

        // 统一透传 Provider 事件 — Provider 发一次，Agent 只负责转发
        match &ev {
            lellm_provider::ProviderEvent::Token { token } => {
                text_buffer.push_str(token);
            }
            lellm_provider::ProviderEvent::ThinkingDelta { thinking, redacted } => {
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

            // Thinking 块优先于 Text（符合 Anthropic 响应顺序）
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
                // 有工具调用 — 追加历史并执行
                state.push_assistant(response.content.clone());
                state.add_tool_calls(pending_tool_calls.len());

                let results =
                    emit_and_execute_tools(tx, executor, &pending_tool_calls, &budget).await;
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

                return Ok(StreamIterResult::Continue {
                    response,
                    tool_calls: pending_tool_calls,
                });
            } else {
                // 无工具调用 — 正常结束（ResponseComplete 已在上方统一透传）
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

/// 流式模式下 emit ToolStart/ToolEnd 并串行执行工具。
///
/// **设计决策（见 docs/DESIGN.md §8）：** 流式模式工具执行强制串行，
/// 即使工具标记为 Safe。原因：ToolStart/ToolEnd 与 Token 交错会让消费者解析更复杂。
/// v0.2 再优化流式分组并发。
async fn emit_and_execute_tools(
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
