//! ToolUseLoop — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。

use std::sync::Arc;

use lellm_core::{ChatRequest, ChatResponse, LlmError, Message};
use lellm_provider::{LlmProvider, StreamEvent, StreamMode};

use super::executor::ToolExecutor;

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

/// 管理 LLM ↔ 工具调用闭环
pub struct ToolUseLoop {
    provider: Arc<dyn LlmProvider>,
    executor: ToolExecutor,
    request: ChatRequest,
    max_iterations: usize,
}

impl ToolUseLoop {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        executor: ToolExecutor,
        request: ChatRequest,
    ) -> Self {
        Self {
            provider,
            executor,
            request,
            max_iterations: 15,
        }
    }

    pub fn set_max_iterations(&mut self, max: usize) -> &mut Self {
        self.max_iterations = max;
        self
    }

    /// 非流式执行
    pub async fn execute(self) -> Result<ToolUseResult, LlmError> {
        let mut req = self.request;

        for iteration in 1..=self.max_iterations {
            let response = self.provider.llm_call(&req).await?;

            if !response.has_tool_calls() {
                return Ok(ToolUseResult {
                    response,
                    messages: req.messages,
                    iterations: iteration,
                });
            }

            let tool_calls = response.extract_tool_calls();

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
            status: 0,
            body: format!(
                "tool-use loop exceeded max iterations ({})",
                self.max_iterations
            ),
        })
    }

    /// 流式执行，返回事件接收器
    pub fn execute_stream(
        self,
        _mode: StreamMode,
    ) -> tokio::sync::mpsc::Receiver<Result<StreamEvent, LlmError>> {
        // TODO: 实现流式执行
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let _ = tx;
        rx
    }
}
