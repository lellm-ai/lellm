//! ToolUseLoop — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。

use std::sync::Arc;

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message};
use lellm_provider::LlmProvider;

use super::executor::ToolExecutor;

/// 解析后的模型 — 绑定 provider + model
#[derive(Clone)]
pub struct ResolvedModel {
    pub provider: Arc<dyn LlmProvider>,
    pub model: String,
}

/// 工具执行结果
#[derive(Debug, Clone)]
pub enum ToolCallResult {
    Ok(String),
    Err(String),
}

/// ToolUseLoop 执行结果
#[derive(Debug)]
pub struct ToolUseResult {
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

    /// 流式执行，返回事件接收器（P4 实现）
    pub fn execute_stream(
        self,
    ) -> tokio::sync::mpsc::Receiver<Result<super::AgentEvent, LlmError>> {
        // TODO: P4 — 实现流式执行
        let (_tx, rx) = tokio::sync::mpsc::channel(32);
        rx
    }
}
