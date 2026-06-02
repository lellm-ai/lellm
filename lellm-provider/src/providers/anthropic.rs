//! Anthropic Provider 适配器。

use async_trait::async_trait;
use lellm_core::{ChatRequest, ChatResponse, LlmError};

use super::base::{HttpRequest, HttpResponse, ProviderAdapter, StreamChunk};

/// Anthropic 适配器。
#[derive(Debug, Clone)]
pub struct AnthropicAdapter;

#[async_trait]
impl ProviderAdapter for AnthropicAdapter {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn build_request(&self, _req: &ChatRequest) -> Result<HttpRequest, LlmError> {
        // TODO: 实现 Anthropic Messages API 请求构建
        Ok(HttpRequest {
            url: String::new(),
            method: "POST".into(),
            headers: Vec::new(),
            body: None,
            stream: false,
        })
    }

    fn parse_response(&self, _resp: &HttpResponse) -> Result<ChatResponse, LlmError> {
        // TODO: 实现 Anthropic 响应解析
        Err(LlmError::ParseError {
            detail: "AnthropicAdapter::parse_response not yet implemented".into(),
        })
    }

    fn parse_stream_chunk(&self, _chunk: &[u8]) -> Option<StreamChunk> {
        // TODO: 实现 Anthropic 流式解析
        None
    }
}

/// Anthropic Provider
pub type AnthropicProvider = super::base::GenericProvider<AnthropicAdapter>;
