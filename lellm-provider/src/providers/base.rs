//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（重试、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的格式转换。

use lellm_core::{ChatRequest, ChatResponse, LlmError, TokenUsage, ToolCall};
use std::collections::HashMap;

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

/// 流式 chunk — Adapter 解析协议后返回
#[derive(Debug)]
pub(crate) enum StreamChunk {
    TextDelta(String),
    ToolCallDelta {
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    Usage(TokenUsage),
    Done,
}

/// 流式解析结果 — 三态
#[derive(Debug)]
pub(crate) enum StreamParseResult {
    Chunk(StreamChunk),
    Empty,
    Done,
}

/// Provider 适配器 trait — 各 provider 只需实现此 trait。
pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn build_request(&self, req: &ChatRequest) -> Result<HttpRequest, LlmError>;
    fn parse_response(&self, resp: &HttpResponse) -> Result<ChatResponse, LlmError>;
    fn parse_stream_chunk(&self, chunk: &[u8]) -> Result<StreamParseResult, LlmError>;
}

/// 通用 Provider，适配任何 ProviderAdapter。
#[allow(private_bounds)]
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

/// ToolCall 增量组装器（GenericProvider 内部使用）
pub(crate) struct ToolCallAccumulator {
    current: HashMap<String, PendingToolCall>,
}

struct PendingToolCall {
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self {
            current: HashMap::new(),
        }
    }

    /// 接收增量数据并组装
    pub fn feed(&mut self, id: &str, name: Option<String>, arguments_delta: String) {
        let entry = self
            .current
            .entry(id.to_string())
            .or_insert_with(|| PendingToolCall {
                name: None,
                arguments: String::new(),
            });
        if let Some(n) = name {
            entry.name = Some(n);
        }
        entry.arguments.push_str(&arguments_delta);
    }

    /// 完成组装，返回完整的 ToolCall 列表
    pub fn finalize(self) -> Result<Vec<ToolCall>, LlmError> {
        let mut result = Vec::new();
        for (id, pending) in self.current {
            let name = pending.name.unwrap_or_else(|| "unknown".to_string());
            let arguments: serde_json::Value = serde_json::from_str(&pending.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(pending.arguments));
            result.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok(result)
    }
}
