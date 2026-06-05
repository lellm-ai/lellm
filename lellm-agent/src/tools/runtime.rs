//! Agent Runtime — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message, ToolResult};
use lellm_provider::ResolvedModel;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

use super::executor::ToolExecutor;
use super::fallback::{DefaultFallback, FallbackAction, FallbackContext, FallbackStrategy};
use super::{AgentEvent, AgentStream, StopReason};

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

/// 将 ToolResult 转为 Message::ToolResult
fn to_tool_result_message(call: &lellm_core::ToolCall, result: ToolResult) -> Message {
    let content_str = match result {
        Ok(s) => s,
        Err(e) => format!("tool error: {e}"),
    };
    Message::ToolResult {
        tool_call_id: call.id.clone(),
        content: lellm_core::text_block(content_str),
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

// ─── ToolUseLoop ───

/// 管理 LLM 与工具调用闭环
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    max_iterations: usize,
    fallback: Arc<dyn FallbackStrategy>,
}

impl ToolUseLoop {
    pub fn new(model: ResolvedModel, executor: ToolExecutor) -> Self {
        Self {
            model,
            executor,
            max_iterations: 15,
            fallback: Arc::new(DefaultFallback::default()),
        }
    }

    pub fn set_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max;
        self
    }

    /// 注入 Fallback 策略（默认 `DefaultFallback::new(3)`）
    pub fn with_fallback(mut self, fallback: Arc<dyn FallbackStrategy>) -> Self {
        self.fallback = fallback;
        self
    }

    /// 非流式执行
    ///
    /// 语义：
    /// - `Ok(ToolUseResult)` — Agent 层完成（含 MaxIterationsReached）
    /// - `Err(LlmError)` — Provider 调用失败
    pub async fn execute(self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let mut state = LoopState::new(messages);
        let mut last_response: Option<ChatResponse> = None;

        loop {
            if state.reached_max(self.max_iterations) {
                return Ok(
                    state.finish_max_iterations(last_response.unwrap_or_else(|| empty_response()))
                );
            }

            state.next_iteration();

            let req = ChatRequest {
                model: self.model.model.clone(),
                messages: state.messages.clone(),
                ..Default::default()
            };

            // Provider 调用 — 受 FallbackStrategy 保护
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
                        match self.fallback.handle(&ctx).await {
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

            // 提取工具调用并执行
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
    pub fn execute_stream(self, messages: Vec<Message>) -> AgentStream {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let model = self.model.clone();
        let executor = self.executor;
        let max_iterations = self.max_iterations;
        let fallback = self.fallback.clone();

        tokio::spawn(async move {
            let mut state = LoopState::new(messages);
            let mut last_response: Option<ChatResponse> = None;

            loop {
                if state.reached_max(max_iterations) {
                    let _ = emit(
                        &tx,
                        AgentEvent::LoopEnd {
                            result: state.finish_max_iterations(
                                last_response.unwrap_or_else(|| empty_response()),
                            ),
                        },
                    )
                    .await;
                    return;
                }

                state.next_iteration();

                let req = ChatRequest {
                    model: model.model.clone(),
                    messages: state.messages.clone(),
                    ..Default::default()
                };

                // Provider 流式调用 — 受 FallbackStrategy 保护
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
                            match fallback.handle(&ctx).await {
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

                        let iter_result = process_stream_iteration(
                            &tx,
                            &executor,
                            &mut state,
                            &mut stream,
                            &mut text_buffer,
                        )
                        .await;

                        // 每轮结束后清空 buffer，避免跨轮次累积
                        text_buffer.clear();

                        if let Some(resp) = iter_result.response {
                            last_response = Some(resp);
                        }

                        if iter_result.terminated {
                            let result = if iter_result.is_complete {
                                state.finish_complete(
                                    last_response.unwrap_or_else(|| empty_response()),
                                )
                            } else {
                                state.finish_max_iterations(
                                    last_response.unwrap_or_else(|| empty_response()),
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
            let content: Vec<lellm_core::ContentBlock> =
                lellm_core::text_block(text_buffer.clone())
                    .into_iter()
                    .chain(
                        pending_tool_calls
                            .iter()
                            .map(|tc| lellm_core::ContentBlock::ToolCall(tc.clone())),
                    )
                    .collect();

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

/// 流式模式下 emit ToolStart/ToolEnd 并串行执行工具
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

        results.push(to_tool_result_message(tc, result));
    }

    Some(results)
}
