//! OpenAI 兼容协议适配器。
//!
//! 覆盖 OpenAI、NVIDIA、DeepSeek、VLLM、LLaMA 等使用 OpenAI 兼容接口的 provider。

use lellm_core::{ChatRequest, ChatResponse, LlmError};

use super::base::{HttpRequest, HttpResponse, ProviderAdapter, StreamParseResult};

/// OpenAI 兼容适配器 — 一个实现覆盖所有 OpenAI 兼容 provider。
#[derive(Debug, Clone)]
pub struct OpenAICompatAdapter {
    /// Provider 标识
    pub provider_id: String,
}

impl OpenAICompatAdapter {
    pub fn openai() -> Self {
        Self {
            provider_id: "openai".into(),
        }
    }

    pub fn nvidia() -> Self {
        Self {
            provider_id: "nvidia".into(),
        }
    }

    pub fn deepseek() -> Self {
        Self {
            provider_id: "deepseek".into(),
        }
    }

    pub fn vllm() -> Self {
        Self {
            provider_id: "vllm".into(),
        }
    }

    pub fn llama() -> Self {
        Self {
            provider_id: "llama".into(),
        }
    }
}

impl ProviderAdapter for OpenAICompatAdapter {
    fn name(&self) -> &str {
        &self.provider_id
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
            detail: "OpenAICompatAdapter::parse_response not yet implemented".into(),
        })
    }

    fn parse_stream_chunk(&self, _chunk: &[u8]) -> Result<StreamParseResult, LlmError> {
        Ok(StreamParseResult::Empty)
    }
}
