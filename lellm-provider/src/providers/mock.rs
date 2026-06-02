//! Mock Provider — 测试用。

use lellm_core::{ChatRequest, ChatResponse, LlmError};

use crate::{LlmProvider, LlmStream, StreamEvent};

/// 测试用 Mock Provider。
pub struct MockProvider {
    pub responses: Vec<ChatResponse>,
    pub received_requests: std::sync::Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            received_requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn reply_with(response: ChatResponse) -> Self {
        Self::new(vec![response])
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockProvider {
    async fn llm_call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.received_requests.lock().unwrap().push(request.clone());

        self.responses.first().cloned().ok_or(LlmError::ApiError {
            status: 500,
            body: "No mock response configured".into(),
        })
    }

    async fn llm_call_stream(&self, _request: &ChatRequest) -> Result<LlmStream, LlmError> {
        // TODO: 实现流式 mock
        Ok(Box::pin(futures_core::stream::empty()))
    }

    fn provider_id(&self) -> &str {
        "mock"
    }
}
