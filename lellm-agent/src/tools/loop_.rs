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
    pub async fn execute(self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let mut req = ChatRequest {
            model: self.model.model.clone(),
            messages,
            ..Default::default()
        };

        for iteration in 1..=self.max_iterations {
            let response = self.model.provider.call(&req).await?;

            if response.tool_calls.is_empty() {
                return Ok(ToolUseResult {
                    stop_reason: StopReason::Complete,
                    response,
                    messages: req.messages,
                    iterations: iteration,
                });
            }

            let tool_calls = response.tool_calls.clone();

            req.messages.push(Message::Assistant {
                content: response.content.clone(),
            });

            let tool_results = self.executor.execute_batch(&tool_calls).await;
            req.messages.extend(tool_results);

            tracing::debug!(
                iteration,
                tool_calls = tool_calls.len(),
                "tool-use loop iteration"
            );
        }

        Err(LlmError::ApiError {
            provider: self.model.provider.provider_id().to_string(),
            status: 0,
            code: None,
            message: format!(
                "tool-use loop exceeded max iterations ({})",
                self.max_iterations
            ),
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

                        let done = loop {
                            match stream.next().await {
                                Some(Ok(lellm_provider::ProviderEvent::Start { .. })) => {
                                    let _ = tx
                                        .send(Ok(AgentEvent::Provider(
                                            lellm_provider::ProviderEvent::Start {
                                                model: model.model.clone(),
                                            },
                                        )))
                                        .await;
                                }
                                Some(Ok(lellm_provider::ProviderEvent::Token { token })) => {
                                    text_buffer.push_str(&token);
                                    let _ = tx
                                        .send(Ok(AgentEvent::Provider(
                                            lellm_provider::ProviderEvent::Token { token },
                                        )))
                                        .await;
                                }
                                Some(Ok(lellm_provider::ProviderEvent::Done {
                                    tool_calls,
                                    usage,
                                })) => {
                                    if !tool_calls.is_empty() {
                                        let content: Vec<lellm_core::ContentBlock> =
                                            lellm_core::text_block(text_buffer.clone())
                                                .into_iter()
                                                .chain(tool_calls.iter().map(|tc| {
                                                    lellm_core::ContentBlock::ToolCall(tc.clone())
                                                }))
                                                .collect();

                                        req.messages.push(Message::Assistant { content });

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
                                        let usage_val = usage.unwrap_or_default();
                                        let response = ChatResponse::new(
                                            lellm_core::text_block(text_buffer),
                                            usage_val,
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
                                                },
                                            }))
                                            .await;

                                        break true;
                                    }
                                }
                                Some(Err(e)) => {
                                    let _ = tx.send(Err(e)).await;
                                    break false;
                                }
                                None => {
                                    break false;
                                }
                            }
                        };

                        if done {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        rx
    }
}
