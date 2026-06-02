//! OpenAI 兼容协议适配器。
//!
//! 覆盖 OpenAI、NVIDIA、DeepSeek、VLLM、LLaMA 等使用 OpenAI 兼容接口的 provider。

use async_trait::async_trait;
use lellm_core::{ChatRequest, ChatResponse, LlmError};

use super::base::{HttpRequest, HttpResponse, ProviderAdapter, StreamChunk};

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

#[async_trait]
impl ProviderAdapter for OpenAICompatAdapter {
    fn name(&self) -> &str {
        &self.provider_id
    }

    fn build_request(&self, _req: &ChatRequest) -> Result<HttpRequest, LlmError> {
        // TODO: 实现 OpenAI 兼容协议的请求构建
        Ok(HttpRequest {
            url: String::new(),
            method: "POST".into(),
            headers: Vec::new(),
            body: None,
            stream: false,
        })
    }

    fn parse_response(&self, _resp: &HttpResponse) -> Result<ChatResponse, LlmError> {
        // TODO: 实现 OpenAI 兼容协议的响应解析
        Err(LlmError::ParseError {
            detail: "OpenAICompatAdapter::parse_response not yet implemented".into(),
        })
    }

    fn parse_stream_chunk(&self, _chunk: &[u8]) -> Option<StreamChunk> {
        // TODO: 实现 SSE 流式解析
        None
    }
}

/// NVIDIA Provider
pub type NVIDIAProvider = super::base::GenericProvider<OpenAICompatAdapter>;
/// DeepSeek Provider
pub type DeepSeekProvider = super::base::GenericProvider<OpenAICompatAdapter>;
/// VLLM Provider
pub type VLLMProvider = super::base::GenericProvider<OpenAICompatAdapter>;
/// LLaMA Provider
pub type LLaMAProvider = super::base::GenericProvider<OpenAICompatAdapter>;
