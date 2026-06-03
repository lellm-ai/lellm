//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（HTTP 发送、重试、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的格式转换。

use async_trait::async_trait;
use futures_util::StreamExt;
use lellm_core::{ChatRequest, ChatResponse, LlmError, TokenUsage, ToolCall};
use secrecy::ExposeSecret;
use std::collections::HashMap;

use crate::{LlmProvider, ProviderEvent, ProviderStream};

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

/// 工具调用增量 — 统一格式，吸收所有 Provider 差异
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ToolCallDelta {
    /// 工具调用在消息中的位置索引（用于聚合）
    pub index: usize,
    /// 工具调用 ID（可能延迟出现）
    pub id: Option<String>,
    /// 工具名称（可能延迟出现）
    pub name: Option<String>,
    /// 参数增量片段（最终拼接为完整 JSON）
    pub arguments_delta: Option<String>,
}

/// 流式 chunk — Adapter 解析协议后返回
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum StreamChunk {
    TextDelta(String),
    ToolCallDelta(ToolCallDelta),
    Usage(TokenUsage),
    Done,
}

/// 流式解析结果 — 可能包含多个 chunk（OpenAI 单行可能有多个 tool_call delta）
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct StreamParseResult {
    pub chunks: Vec<StreamChunk>,
}

impl StreamParseResult {
    pub fn empty() -> Self {
        Self { chunks: Vec::new() }
    }

    pub fn chunk(c: StreamChunk) -> Self {
        Self { chunks: vec![c] }
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// SSE 事件 — 由 GenericProvider 统一解析 SSE 协议后传递给 Adapter。
#[derive(Debug, Clone)]
pub(crate) struct SseEvent {
    /// SSE event 类型（如 "content_block_delta"），可能为空
    pub event: Option<String>,
    /// SSE data 内容（通常是 JSON 字符串）
    pub data: String,
}

/// Provider 适配器 trait — 各 provider 只需实现此 trait。
#[allow(dead_code)]
pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn build_request(
        &self,
        req: &ChatRequest,
        config: &ProviderConfig,
        stream: bool,
    ) -> Result<HttpRequest, LlmError>;
    fn parse_response(&self, resp: &HttpResponse) -> Result<ChatResponse, LlmError>;
    /// 解析单个 SSE 事件的 data 字段（JSON 字符串），返回解析后的 chunk。
    /// SSE 协议解析（缓冲、行拆分、event/data 提取）由 GenericProvider 统一处理。
    fn parse_stream_chunk(&self, event: &SseEvent) -> Result<StreamParseResult, LlmError>;
}

/// 通用 Provider，适配任何 ProviderAdapter。
///
/// Adapter 必须 Clone，以便在流式调用时克隆进 tokio::spawn。
#[allow(private_bounds)]
pub struct GenericProvider<A: ProviderAdapter> {
    adapter: A,
    client: reqwest::Client,
    config: ProviderConfig,
}

#[allow(private_bounds)]
impl<A: ProviderAdapter + Clone> GenericProvider<A> {
    pub fn new(adapter: A, config: ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .user_agent(format!("LeLLM/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();

        Self {
            adapter,
            client,
            config,
        }
    }

    /// 将内部 HttpRequest 转为 reqwest RequestBuilder
    fn build_reqwest(&self, http_req: &HttpRequest) -> reqwest::RequestBuilder {
        let builder = self.client.request(
            http_req.method.parse().unwrap_or(reqwest::Method::POST),
            &http_req.url,
        );
        let builder = http_req
            .headers
            .iter()
            .fold(builder, |b, (k, v)| b.header(k, v));
        match &http_req.body {
            Some(bytes) => builder.body(bytes.clone()),
            None => builder,
        }
    }

    /// 发送 reqwest Request 并返回 HttpResponse 或 LlmError
    async fn send_request(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<HttpResponse, LlmError> {
        let resp = builder.send().await.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|sv| (k.to_string(), sv.to_string())))
            .collect();
        let body = resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })?;

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[async_trait]
#[allow(private_bounds)]
impl<A: ProviderAdapter + Clone + 'static> LlmProvider for GenericProvider<A> {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let http_req = self.adapter.build_request(request, &self.config, false)?;
        let builder = self.build_reqwest(&http_req);
        let http_resp = self.send_request(builder).await?;

        // 4xx/5xx 转为 ApiError
        if http_resp.status >= 400 {
            let body_str = String::from_utf8_lossy(&http_resp.body);
            return Err(LlmError::ApiError {
                provider: self.adapter.name().to_string(),
                status: http_resp.status,
                code: None,
                message: body_str.into_owned(),
            });
        }

        self.adapter.parse_response(&http_resp)
    }

    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        let http_req = self.adapter.build_request(request, &self.config, true)?;
        let builder = self.build_reqwest(&http_req);

        let resp = builder.send().await.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.bytes().await.map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })?;
            let body_str = String::from_utf8_lossy(&body);
            return Err(LlmError::ApiError {
                provider: self.adapter.name().to_string(),
                status,
                code: None,
                message: body_str.into_owned(),
            });
        }

        let model = request.model.clone();
        let adapter = self.adapter.clone();

        // 使用 mpsc channel 桥接 reqwest Stream 到 ProviderStream
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let stream = resp.bytes_stream();
        let mut boxed_stream = Box::pin(stream);

        tokio::spawn(async move {
            let _ = tx.send(Ok(ProviderEvent::Start { model })).await;

            let mut accumulator = ToolCallAccumulator::new();
            let mut usage: Option<TokenUsage> = None;
            let mut is_done = false;

            // SSE 缓冲区 — bytes_stream() 可能截断单条 SSE 消息
            let mut sse_buffer = String::new();
            // 当前 SSE 事件的 event 类型
            let mut current_event: Option<String> = None;

            while let Some(result) = boxed_stream.next().await {
                match result {
                    Ok(bytes) => {
                        let chunk_str = String::from_utf8_lossy(&bytes).to_string();
                        sse_buffer.push_str(&chunk_str);

                        loop {
                            match sse_buffer.find('\n') {
                                Some(end_pos) => {
                                    let line = sse_buffer[..end_pos].to_string();
                                    sse_buffer.replace_range(..=end_pos, "");

                                    let line_trimmed = line.trim();

                                    if line_trimmed.is_empty() {
                                        // 空行表示一条 SSE 消息结束，重置 event
                                        current_event = None;
                                        continue;
                                    }

                                    if let Some(value) = line_trimmed.strip_prefix("event:") {
                                        current_event = Some(value.trim().to_string());
                                        continue;
                                    }

                                    if let Some(data) = line_trimmed.strip_prefix("data:") {
                                        let data = data.trim().to_string();
                                        let sse_event = SseEvent {
                                            event: current_event.take(),
                                            data,
                                        };

                                        match adapter.parse_stream_chunk(&sse_event) {
                                            Ok(result) => {
                                                for c in result.chunks {
                                                    match c {
                                                        StreamChunk::TextDelta(text) => {
                                                            let _ = tx
                                                                .send(Ok(ProviderEvent::Token {
                                                                    token: text,
                                                                }))
                                                                .await;
                                                        }
                                                        StreamChunk::ToolCallDelta(delta) => {
                                                            accumulator.feed(&delta);
                                                        }
                                                        StreamChunk::Usage(u) => {
                                                            usage = Some(u);
                                                        }
                                                        StreamChunk::Done => {
                                                            is_done = true;
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                let _ = tx.send(Err(e)).await;
                                                break;
                                            }
                                        }
                                    }
                                    // 其他字段（如 id:, retry:）忽略
                                }
                                None => {
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(LlmError::Network {
                                detail: e.to_string(),
                            }))
                            .await;
                        break;
                    }
                }

                if is_done {
                    break;
                }
            }

            let tool_calls = accumulator.finalize().unwrap_or_default();
            let _ = tx.send(Ok(ProviderEvent::Done { tool_calls, usage })).await;
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        Ok(Box::pin(rx_stream))
    }

    fn provider_id(&self) -> &str {
        self.adapter.name()
    }
}

/// Provider 配置。
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    /// API 基础地址
    pub base_url: url::Url,
    /// 认证配置
    pub auth: AuthConfig,
    /// 默认模型名称
    pub model: String,
    /// 请求超时
    pub timeout: std::time::Duration,
}

impl ProviderConfig {
    /// 获取有效模型名 — 优先使用 request.model，回退到 config.model
    pub fn effective_model<'a>(&'a self, request_model: &'a str) -> std::borrow::Cow<'a, str> {
        if request_model.is_empty() {
            std::borrow::Cow::Borrowed(&self.model)
        } else {
            std::borrow::Cow::Borrowed(request_model)
        }
    }

    /// 便捷构造 — Bearer 认证（OpenAI 等大多数 Provider）
    pub fn bearer(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::Bearer {
                api_key: secrecy::SecretString::new(api_key.into()),
            },
            model: model.into(),
            timeout: std::time::Duration::from_secs(120),
        })
    }

    /// 便捷构造 — 自定义 Header 认证（Anthropic 等）
    pub fn header(
        base_url: impl AsRef<str>,
        header: impl Into<String>,
        value: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::Header {
                header: header.into(),
                value: secrecy::SecretString::new(value.into()),
            },
            model: model.into(),
            timeout: std::time::Duration::from_secs(120),
        })
    }

    /// 便捷构造 — 无认证（本地调试）
    pub fn none(
        base_url: impl AsRef<str>,
        model: impl Into<String>,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::None,
            model: model.into(),
            timeout: std::time::Duration::from_secs(120),
        })
    }

    /// 修改认证配置
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    /// 修改超时
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: url::Url::parse("http://localhost").unwrap(),
            auth: AuthConfig::None,
            model: String::new(),
            timeout: std::time::Duration::from_secs(120),
        }
    }
}

/// 认证配置。
#[derive(Clone, Debug)]
pub enum AuthConfig {
    /// Bearer Token 认证（OpenAI 等）
    Bearer { api_key: secrecy::SecretString },
    /// 自定义 Header 认证（Anthropic 等）
    Header {
        header: String,
        value: secrecy::SecretString,
    },
    /// 无认证（本地调试等）
    None,
}

impl AuthConfig {
    /// 获取认证 header，返回 `(header_name, header_value)`
    pub fn get_header(&self) -> Option<(String, String)> {
        match self {
            AuthConfig::Bearer { api_key } => Some((
                "Authorization".to_string(),
                format!("Bearer {}", api_key.expose_secret()),
            )),
            AuthConfig::Header { header, value } => {
                Some((header.clone(), value.expose_secret().to_string()))
            }
            AuthConfig::None => None,
        }
    }
}

/// ToolCall 增量组装器 — 按 index 聚合（GenericProvider 内部使用）
///
/// 以 index 为 key，因为很多 Provider 的第一批 delta 只有 index 而没有 id。
#[allow(dead_code)]
pub(crate) struct ToolCallAccumulator {
    current: HashMap<usize, PendingToolCall>,
}

#[allow(dead_code)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[allow(dead_code)]
impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self {
            current: HashMap::new(),
        }
    }

    /// 接收统一的 ToolCallDelta 增量并组装
    pub fn feed(&mut self, delta: &ToolCallDelta) {
        let entry = self
            .current
            .entry(delta.index)
            .or_insert_with(|| PendingToolCall {
                id: None,
                name: None,
                arguments: String::new(),
            });

        if let Some(ref id) = delta.id {
            entry.id = Some(id.clone());
        }
        if let Some(ref name) = delta.name {
            entry.name = Some(name.clone());
        }
        if let Some(ref d) = delta.arguments_delta {
            entry.arguments.push_str(d);
        }
    }

    /// 完成组装，返回完整的 ToolCall 列表（按 index 排序）
    pub fn finalize(self) -> Result<Vec<ToolCall>, LlmError> {
        let mut entries: Vec<_> = self.current.into_iter().collect();
        entries.sort_by_key(|&(idx, _)| idx);

        let mut result = Vec::new();
        for (_index, pending) in entries {
            let id = pending.id.unwrap_or_else(|| "unknown".to_string());
            let name = pending.name.unwrap_or_else(|| "unknown".to_string());
            let arguments: serde_json::Value = if pending.arguments.is_empty() {
                serde_json::Value::Object(Default::default())
            } else {
                serde_json::from_str(&pending.arguments)
                    .unwrap_or(serde_json::Value::String(pending.arguments))
            };
            result.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok(result)
    }
}
