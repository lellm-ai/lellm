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
}

impl Default for ToolUseConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_iterations: 10,
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
    messages: Vec<Message>,
) -> ChatRequest {
    ChatRequest {
        model: model.model.clone(),
        messages,
        tools: executor.has_tools().then(|| executor.definitions()),
        ..Default::default()
    }
}

// ─── 循环状态机 ───

/// Agent Loop 共享状态 — 非流式与流式执行模式共用。
#[derive(Debug)]
pub struct LoopState {
    pub messages: Vec<Message>,
    pub iterations: usize,
    pub tool_calls_executed: usize,
}

impl LoopState {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            iterations: 0,
            tool_calls_executed: 0,
        }
    }

    /// 追加 Assistant 响应到历史
    pub fn push_assistant(&mut self, content: Vec<lellm_core::ContentBlock>) {
        self.messages.push(Message::Assistant { content });
    }

    /// 追加工具执行结果到历史
    pub fn push_tool_results(&mut self, results: Vec<Message>) {
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
/// **高级（直接构造 + with_ 链式调用）：**
/// ```rust,ignore
/// let agent = ToolUseLoop::new(model, executor, config, deps)
///     .with_system_prompt("你是助手".into());
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

    // ─── with_ 糖衣 setter（次要入口，链式调用）───────────────

    /// 设置系统提示（覆盖 config.system_prompt）
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.config.system_prompt = Some(prompt);
        self
    }

    /// 设置最大迭代轮次（覆盖 config.max_iterations）
    pub fn with_max_iterations(mut self, max: usize) -> Self {
        self.config.max_iterations = max;
        self
    }

    // ─── 内部方法 ─────────────────────────────────────────────

    /// 构建有效的请求消息列表：system_prompt + conversation_messages。
    /// 双来源冲突时返回 DuplicateSystemPrompt 错误。
    fn build_request_messages(&self, messages: &[Message]) -> Result<Vec<Message>, LlmError> {
        build_request_messages_inner(&self.config, messages)
    }

    /// 构建 ChatRequest，自动注入工具 Schema。
    fn build_request(&self, messages: Vec<Message>) -> ChatRequest {
        build_request_inner(&self.model, &self.executor, messages)
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

        loop {
            if state.reached_max(self.config.max_iterations) {
                return Ok(
                    state.finish_max_iterations(last_response.unwrap_or_else(empty_response))
                );
            }

            state.next_iteration();

            let req = self.build_request(state.messages.clone());

            let mut retry_attempt: usize = 1;
            let response = loop {
                match self.model.provider.call(&req).await {
                    Ok(resp) => break resp,
                    Err(e) => {
                        tracing::warn!(
                            attempt = retry_attempt,
                            error = %e,
                            "provider call failed, fallback handling"
                        );
                        let ctx = FallbackContext {
                            error: e,
                            attempt: retry_attempt,
                            iterations: state.iterations,
                            conversation: state.messages.clone().into(),
                        };
                        match self.deps.fallback.handle(&ctx).await {
                            FallbackAction::Retry => {
                                retry_attempt += 1;
                                continue;
                            }
                            FallbackAction::Abort => {
                                return Err(ctx.error);
                            }
                        }
                    }
                }
            };
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

            let results = self.executor.execute_batch(&tool_calls).await;
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

                let req = build_request_inner(&model, &executor, state.messages.clone());

                let mut retry_attempt: usize = 1;
                let stream_result: Result<lellm_provider::ProviderStream, LlmError> = loop {
                    match model.provider.stream(&req).await {
                        Ok(s) => break Ok(s),
                        Err(e) => {
                            tracing::warn!(
                                attempt = retry_attempt,
                                error = %e,
                                "provider stream failed, fallback handling"
                            );
                            let ctx = FallbackContext {
                                error: e,
                                attempt: retry_attempt,
                                iterations: state.iterations,
                                conversation: state.messages.clone().into(),
                            };
                            match deps.fallback.handle(&ctx).await {
                                FallbackAction::Retry => {
                                    retry_attempt += 1;
                                    continue;
                                }
                                FallbackAction::Abort => {
                                    break Err(ctx.error);
                                }
                            }
                        }
                    }
                };

                match stream_result {
                    Ok(mut stream) => {
                        let mut text_buffer = String::new();
                        let mut thinking_buffer = String::new();

                        let iter_result = process_stream_iteration(
                            &tx,
                            &executor,
                            &mut state,
                            &mut stream,
                            &mut text_buffer,
                            &mut thinking_buffer,
                        )
                        .await;

                        // 每轮结束后清空 buffer，避免跨轮次累积
                        text_buffer.clear();
                        thinking_buffer.clear();

                        if let Some(resp) = iter_result.response {
                            last_response = Some(resp);
                        }

                        if iter_result.terminated {
                            let result = if iter_result.is_complete {
                                state.finish_complete(last_response.unwrap_or_else(empty_response))
                            } else {
                                state.finish_max_iterations(
                                    last_response.unwrap_or_else(empty_response),
                                )
                            };
                            let _ = emit(&tx, AgentEvent::LoopEnd { result }).await;
                            return;
                        }
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

/// 流式单轮迭代的结果
struct StreamIterResult {
    /// 是否应退出外层循环
    terminated: bool,
    /// 是否正常完成（vs 错误终止）
    is_complete: bool,
    /// 本轮构建的 ChatResponse（供外层追踪 last_response）
    response: Option<ChatResponse>,
}

/// 处理流式单轮迭代
async fn process_stream_iteration(
    tx: &Sender<AgentEvent>,
    executor: &ToolExecutor,
    state: &mut LoopState,
    stream: &mut lellm_provider::ProviderStream,
    text_buffer: &mut String,
    thinking_buffer: &mut String,
) -> StreamIterResult {
    use futures_util::StreamExt;

    while let Some(result) = stream.next().await {
        let ev = match result {
            Ok(ev) => ev,
            Err(e) => {
                let _ = emit(
                    tx,
                    AgentEvent::LoopError {
                        error: e,
                        iterations: state.iterations,
                    },
                )
                .await;
                return StreamIterResult::terminated_error();
            }
        };

        // 统一透传 Provider 事件 — Provider 发一次，Agent 只负责转发
        match &ev {
            lellm_provider::ProviderEvent::Token { token } => {
                text_buffer.push_str(token);
            }
            lellm_provider::ProviderEvent::ThinkingDelta { thinking } => {
                thinking_buffer.push_str(thinking);
            }
            lellm_provider::ProviderEvent::Start { .. }
            | lellm_provider::ProviderEvent::ResponseComplete { .. } => {}
        }

        if !emit(tx, AgentEvent::Provider(ev.clone())).await {
            return StreamIterResult::terminated_false();
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
                        redacted: None,
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

                let results = emit_and_execute_tools(tx, executor, &pending_tool_calls).await;
                if results.is_none() {
                    return StreamIterResult::terminated_false();
                }
                state.push_tool_results(results.unwrap());

                tracing::debug!(
                    iteration = state.iterations,
                    tool_calls = pending_tool_calls.len(),
                    "tool-use stream iteration"
                );

                return StreamIterResult::new(false, Some(response));
            } else {
                // 无工具调用 — 正常结束（ResponseComplete 已在上方统一透传）
                if !emit(
                    tx,
                    AgentEvent::LoopEnd {
                        result: state.finish_complete(response),
                    },
                )
                .await
                {
                    return StreamIterResult::terminated_false();
                }

                return StreamIterResult::completed(None);
            }
        }
    }

    StreamIterResult::terminated_false()
}

impl StreamIterResult {
    fn new(terminated: bool, response: Option<ChatResponse>) -> Self {
        Self {
            terminated,
            is_complete: false,
            response,
        }
    }
    fn completed(response: Option<ChatResponse>) -> Self {
        Self {
            terminated: true,
            is_complete: true,
            response,
        }
    }
    fn terminated_error() -> Self {
        Self {
            terminated: true,
            is_complete: false,
            response: None,
        }
    }
    fn terminated_false() -> Self {
        Self {
            terminated: false,
            is_complete: false,
            response: None,
        }
    }
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

        let result = executor.execute(tc).await;

        if !emit(
            tx,
            AgentEvent::ToolEnd {
                tool_call_id: tc.id.clone(),
                result: result.clone(),
            },
        )
        .await
        {
            return None;
        }

        results.push(Message::tool_result(tc, &result));
    }

    Some(results)
}
