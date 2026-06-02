//! Anthropic Provider 适配器。

use lellm_core::{ChatRequest, ChatResponse, LlmError};

use super::base::{HttpRequest, HttpResponse, ProviderAdapter, StreamParseResult};

/// Anthropic 适配器。
#[derive(Debug, Clone)]
pub struct AnthropicAdapter;

impl ProviderAdapter for AnthropicAdapter {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn build_request(&self, _req: &ChatRequest) -> Result<HttpRequest, LlmError> {
        Ok(HttpRequest {
            url: String::new(),
            method: "POST".into(),
            headers: Vec::new(),
            body: None,
            stream: false,
        })
    }

    fn parse_response(&self, _resp: &HttpResponse) -> Result<ChatResponse, LlmError> {
        Err(LlmError::ParseError {
            detail: "AnthropicAdapter::parse_response not yet implemented".into(),
        })
    }

    fn parse_stream_chunk(&self, _chunk: &[u8]) -> Result<StreamParseResult, LlmError> {
        Ok(StreamParseResult::Empty)
    }
}
