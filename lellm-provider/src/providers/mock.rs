//! Mock Provider — 测试用。

use std::sync::Mutex;

use async_trait::async_trait;
use futures_util::stream;
use lellm_core::{ChatRequest, ChatResponse, LlmError};

use crate::{LlmProvider, ProviderEvent, ProviderStream};

/// 测试用 Mock Provider。
pub struct MockProvider {
    responses: Vec<ChatResponse>,
    received_requests: Mutex<Vec<ChatRequest>>,
}

impl MockProvider {
    pub fn new(responses: Vec<ChatResponse>) -> Self {
        Self {
            responses,
            received_requests: Mutex::new(Vec::new()),
        }
    }

    pub fn reply_with(response: ChatResponse) -> Self {
        Self::new(vec![response])
    }

    pub fn received_requests(&self) -> Vec<ChatRequest> {
        self.received_requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.received_requests.lock().unwrap().push(request.clone());

        self.responses.first().cloned().ok_or(LlmError::ApiError {
            provider: "mock".into(),
            status: 500,
            code: None,
            message: "No mock response configured".into(),
        })
    }

    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        self.received_requests.lock().unwrap().push(request.clone());

        let response = self.responses.first().cloned().ok_or(LlmError::ApiError {
            provider: "mock".into(),
            status: 500,
            code: None,
            message: "No mock response configured".into(),
        })?;

        let model = String::new();
        let events: Vec<Result<ProviderEvent, LlmError>> = vec![
            Ok(ProviderEvent::Start {
                model: model.clone(),
            }),
            Ok(ProviderEvent::Token {
                token: response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        lellm_core::ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            }),
            Ok(ProviderEvent::Done {
                tool_calls: response.tool_calls.clone(),
                usage: Some(response.usage),
            }),
        ];

        let stream = stream::iter(events);
        Ok(Box::pin(stream))
    }

    fn provider_id(&self) -> &str {
        "mock"
    }
}
