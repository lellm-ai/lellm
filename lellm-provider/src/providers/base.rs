//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（重试、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的格式转换。

use async_trait::async_trait;
use lellm_core::{ChatRequest, ChatResponse, LlmError};

/// Provider 适配器 trait — 各 provider 只需实现此 trait。
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Provider 名称
    fn name(&self) -> &str;

    /// 构建 HTTP 请求
    fn build_request(&self, req: &ChatRequest) -> Result<HttpRequest, LlmError>;

    /// 解析非流式响应
    fn parse_response(&self, resp: &HttpResponse) -> Result<ChatResponse, LlmError>;

    /// 解析流式 chunk，返回 None 表示结束
    fn parse_stream_chunk(&self, chunk: &[u8]) -> Option<StreamChunk>;
}

/// HTTP 请求（provider 构建，GenericProvider 发送）
#[derive(Debug)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub stream: bool,
}

/// HTTP 响应
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 流式 chunk
#[derive(Debug)]
pub enum StreamChunk {
    Token(String),
    ToolCall(lellm_core::ToolCall),
    Usage(lellm_core::TokenUsage),
    Done,
}

/// 通用 Provider，适配任何 ProviderAdapter。
pub struct GenericProvider<A: ProviderAdapter> {
    #[allow(dead_code)]
    adapter: A,
    #[allow(dead_code)]
    client: reqwest::Client,
    #[allow(dead_code)]
    config: ProviderConfig,
}

impl<A: ProviderAdapter> GenericProvider<A> {
    pub fn new(adapter: A, config: ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();

        Self {
            adapter,
            client,
            config,
        }
    }
}

/// Provider 配置。
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            timeout_secs: 120,
        }
    }
}
