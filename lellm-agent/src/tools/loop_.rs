//! ToolUseLoop — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message};
use lellm_provider::ResolvedModel;

use super::executor::ToolExecutor;
use super::{AgentEvent, AgentStream, StopReason};

/// 工具执行结果
#[derive(Debug, Clone)]
pub enum ToolCallResult {
    Ok(String),
    Err(String),
}

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

/// 管理 LLM 与工具调用闭环
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    max_iterations: usize,
}

impl ToolUseLoop {
    pub fn new(model: ResolvedModel, executor: ToolExecutor) -> Self {
        Self {
            model,
            executor,
            max_iterations: 15,
        }
    }

    pub fn set_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max;
        self
    }

    /// 非流式执行
    ///
    /// 语义：
    /// - `Ok(ToolUseResult)` — Agent 层完成（含 MaxIterationsReached）
    /// - `Err(LlmError)` — Provider 调用失败
    pub async fn execute(self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let mut req = ChatRequest {
            model: self.model.model.clone(),
            messages,
            ..Default::default()
        };

        let mut tool_calls_executed = 0usize;
        let mut last_response: Option<ChatResponse> = None;

        for iteration in 1..=self.max_iterations {
            let response = self.model.provider.call(&req).await?;
            last_response = Some(response);

            if last_response.as_ref().unwrap().tool_calls.is_empty() {
                return Ok(ToolUseResult {
                    stop_reason: StopReason::Complete,
                    response: last_response.unwrap(),
                    messages: req.messages,
                    iterations: iteration,
                    tool_calls_executed,
                });
            }

            let tool_calls = last_response.as_ref().unwrap().tool_calls.clone();
            tool_calls_executed += tool_calls.len();

            req.messages.push(Message::Assistant {
                content: last_response.as_ref().unwrap().content.clone(),
            });

            let tool_results = self.executor.execute_batch(&tool_calls).await;
            req.messages.extend(tool_results);

            tracing::debug!(
                iteration,
                tool_calls = tool_calls.len(),
                "tool-use loop iteration"
            );
        }

        // 达到最大轮次 — Agent 层正常终止，不是 Provider 错误
        Ok(ToolUseResult {
            stop_reason: StopReason::MaxIterationsReached,
            response: last_response.unwrap(),
            messages: req.messages,
            iterations: self.max_iterations,
            tool_calls_executed,
        })
    }

    /// 流式执行，返回事件接收器
    ///
    /// 契约：
    /// - 成功路径：发送事件（含 `LoopEnd`），然后 channel 关闭
    /// - 失败路径：发送 `Err(LlmError)`，然后 channel 关闭
    /// - 绝不会发送伪造的 `ToolEnd { tool_call_id: "", .. }`
    pub fn execute_stream(self, messages: Vec<Message>) -> AgentStream {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let model = self.model.clone();
        let executor = self.executor;
        let max_iterations = self.max_iterations;

        tokio::spawn(async move {
            let mut req = ChatRequest {
                model: model.model.clone(),
                messages,
                ..Default::default()
            };

            let mut tool_calls_executed = 0usize;
            let mut last_text = String::new();
            let mut last_usage = lellm_core::TokenUsage::default();
            let mut completed = false;

            for iteration in 1..=max_iterations {
                let _ = tx
                    .send(Ok(AgentEvent::Provider(
                        lellm_provider::ProviderEvent::Start {
                            model: model.model.clone(),
                        },
                    )))
                    .await;

                match model.provider.stream(&req).await {
                    Ok(stream) => {
                        use futures_util::StreamExt;
                        let mut stream = stream;
                        let mut text_buffer = String::new();

                        let mut iteration_over = false;

                        while let Some(event) = stream.next().await {
                            match event {
                                Ok(lellm_provider::ProviderEvent::Start { .. }) => {
                                    let _ = tx
                                        .send(Ok(AgentEvent::Provider(
                                            lellm_provider::ProviderEvent::Start {
                                                model: model.model.clone(),
                                            },
                                        )))
                                        .await;
                                }
                                Ok(lellm_provider::ProviderEvent::Token { token }) => {
                                    text_buffer.push_str(&token);
                                    let _ = tx
                                        .send(Ok(AgentEvent::Provider(
                                            lellm_provider::ProviderEvent::Token { token },
                                        )))
                                        .await;
                                }
                                Ok(lellm_provider::ProviderEvent::Done { tool_calls, usage }) => {
                                    last_text = text_buffer.clone();
                                    last_usage = usage.unwrap_or_default();

                                    if !tool_calls.is_empty() {
                                        let content: Vec<lellm_core::ContentBlock> =
                                            lellm_core::text_block(text_buffer.clone())
                                                .into_iter()
                                                .chain(tool_calls.iter().map(|tc| {
                                                    lellm_core::ContentBlock::ToolCall(tc.clone())
                                                }))
                                                .collect();

                                        req.messages.push(Message::Assistant { content });
                                        tool_calls_executed += tool_calls.len();

                                        let mut tool_results = Vec::new();
                                        for tc in &tool_calls {
                                            let _ = tx
                                                .send(Ok(AgentEvent::ToolStart {
                                                    tool_call_id: tc.id.clone(),
                                                    name: tc.name.clone(),
                                                }))
                                                .await;

                                            let result = executor.execute(tc).await;

                                            let _ = tx
                                                .send(Ok(AgentEvent::ToolEnd {
                                                    tool_call_id: tc.id.clone(),
                                                    result: result.clone(),
                                                }))
                                                .await;

                                            let content_str = match &result {
                                                ToolCallResult::Ok(s) => s.clone(),
                                                ToolCallResult::Err(e) => {
                                                    format!("tool error: {e}")
                                                }
                                            };

                                            tool_results.push(Message::ToolResult {
                                                tool_call_id: tc.id.clone(),
                                                content: lellm_core::text_block(content_str),
                                            });
                                        }
                                        req.messages.extend(tool_results);

                                        tracing::debug!(
                                            iteration,
                                            tool_calls = tool_calls.len(),
                                            "tool-use stream iteration"
                                        );
                                    } else {
                                        let response = ChatResponse::new(
                                            lellm_core::text_block(text_buffer.clone()),
                                            last_usage,
                                            serde_json::Value::Null,
                                        );

                                        let _ = tx
                                            .send(Ok(AgentEvent::Provider(
                                                lellm_provider::ProviderEvent::Done {
                                                    tool_calls: Vec::new(),
                                                    usage: Some(response.usage),
                                                },
                                            )))
                                            .await;

                                        let _ = tx
                                            .send(Ok(AgentEvent::LoopEnd {
                                                result: ToolUseResult {
                                                    stop_reason: StopReason::Complete,
                                                    response,
                                                    messages: req.messages.clone(),
                                                    iterations: iteration,
                                                    tool_calls_executed,
                                                },
                                            }))
                                            .await;

                                        completed = true;
                                        iteration_over = true;
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(e)).await;
                                    iteration_over = true;
                                    break;
                                }
                            }
                        }

                        if iteration_over {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }

            // 达到最大轮次 — 仅在未完成时发送
            if !completed {
                let response = ChatResponse::new(
                    lellm_core::text_block(last_text),
                    last_usage,
                    serde_json::Value::Null,
                );
                let _ = tx
                    .send(Ok(AgentEvent::LoopEnd {
                        result: ToolUseResult {
                            stop_reason: StopReason::MaxIterationsReached,
                            response,
                            messages: req.messages,
                            iterations: max_iterations,
                            tool_calls_executed,
                        },
                    }))
                    .await;
            }
        });

        rx
    }
}
