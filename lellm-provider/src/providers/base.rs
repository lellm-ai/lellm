//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（HTTP 发送、认证、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的协议格式转换。

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::HeaderMap;
use lellm_core::{ChatRequest, ChatResponse, LlmError, TokenUsage, ToolCall};
use secrecy::ExposeSecret;
use std::borrow::Cow;

use crate::{LlmProvider, ProviderEvent, ProviderStream};

// ─── Adapter → GenericProvider 的中间表示 ───

/// Provider 请求 — Adapter 构建，GenericProvider 发送。
///
/// Adapter 只关心协议适配（路径、Header、Body），
/// 不关心 base_url、认证、HTTP Client。
#[derive(Debug)]
pub(crate) struct ProviderRequest {
    pub path: Cow<'static, str>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// HTTP 原始响应 — GenericProvider 接收，4xx/5xx 由 GenericProvider 处理。
#[derive(Debug)]
pub(crate) struct RawResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

// ─── 流式解析中间表示 ───

/// 工具调用增量 — 统一格式，吸收所有 Provider 差异
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
#[derive(Debug)]
pub(crate) enum StreamChunk {
    TextDelta(String),
    ToolCallDelta(ToolCallDelta),
    Usage(TokenUsage),
    Done,
}

/// SSE 事件 — GenericProvider 从字节流中构建，Adapter 只解析 data 字段。
#[derive(Debug, Clone)]
pub(crate) struct SseEvent {
    /// event 字段（可选），如 "message_start", "content_block_delta"
    pub event: Option<String>,
    /// data 字段内容（通常是 JSON 字符串或标记如 "[DONE]"）
    pub data: String,
}

/// 流式解析结果 — 可能包含多个 chunk
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
}

// ─── ProviderAdapter SPI (pub(crate)) ───

/// Provider 适配器 trait — 各 provider 只需实现此 trait。
///
/// Adapter **不知道** ProviderConfig、reqwest、HTTP。
/// 只负责：ChatRequest → ProviderRequest（请求），body bytes → ChatResponse（响应）。
pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;

    /// 构建 Provider 请求（路径 + 协议 Header + JSON Body 字节）
    fn build_request(&self, req: &ChatRequest, stream: bool) -> Result<ProviderRequest, LlmError>;

    /// 解析成功响应 body（2xx）为 ChatResponse
    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;

    /// 解析单个 SSE 事件的 data 字段。
    /// SSE 协议解析（缓冲、行拆分、event/data 提取）由 GenericProvider 统一处理，构建 SseEvent。
    fn parse_stream_chunk(&self, event: &SseEvent) -> Result<StreamParseResult, LlmError>;
}

// ─── GenericProvider — 持有 config + client，统一 HTTP 传输 ───

/// 通用 Provider，适配任何 ProviderAdapter。
///
/// Adapter 必须 Clone，以便在流式调用时克隆进 tokio::spawn。
#[allow(private_bounds)]
pub struct GenericProvider<A: ProviderAdapter> {
    adapter: A,
    config: ProviderConfig,
    client: reqwest::Client,
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
            config,
            client,
        }
    }

    /// 构建 reqwest RequestBuilder — URL + Auth + Headers + Body 统一在此组装。
    ///
    /// 这是 RetryPolicy、Metrics、OpenTelemetry 的唯一接入点。
    fn build_request_builder(
        &self,
        req: &ProviderRequest,
    ) -> Result<reqwest::RequestBuilder, LlmError> {
        let url = self
            .config
            .base_url
            .join(&req.path as &str)
            .map_err(|e| LlmError::Network {
                detail: format!("Invalid URL: {}", e),
            })?;

        let builder = self.client.post(url);
        let builder = self.config.auth.apply(builder);

        // 注入 adapter 协议 header
        let builder = req
            .headers
            .iter()
            .fold(builder, |b, (key, value)| b.header(key, value));

        Ok(builder.body(req.body.clone()))
    }

    /// 发送请求并返回 RawResponse（一次性读取 body）
    async fn send(&self, req: ProviderRequest) -> Result<RawResponse, LlmError> {
        let builder = self.build_request_builder(&req)?;
        let resp = builder.send().await.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        Ok(RawResponse {
            status,
            headers,
            body,
        })
    }

    /// 统一处理 4xx / 5xx 错误响应
    fn handle_error(&self, resp: &RawResponse) -> LlmError {
        let body_str = String::from_utf8_lossy(&resp.body);
        match resp.status {
            401 => LlmError::Authentication {
                provider: self.adapter.name().to_string(),
                message: body_str.into_owned(),
            },
            429 => LlmError::RateLimited {
                provider: self.adapter.name().to_string(),
            },
            status @ (400..=599) => LlmError::ApiError {
                provider: self.adapter.name().to_string(),
                status,
                code: None,
                message: body_str.into_owned(),
            },
            _ => LlmError::ApiError {
                provider: self.adapter.name().to_string(),
                status: resp.status,
                code: None,
                message: format!("Unexpected status: {}", resp.status),
            },
        }
    }
}

#[async_trait]
#[allow(private_bounds)]
impl<A: ProviderAdapter + Clone + 'static> LlmProvider for GenericProvider<A> {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let http_req = self.adapter.build_request(request, false)?;
        let resp = self.send(http_req).await?;

        if (200..=299).contains(&resp.status) {
            self.adapter.parse_response(&resp.body)
        } else {
            Err(self.handle_error(&resp))
        }
    }

    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        let http_req = self.adapter.build_request(request, true)?;

        // 复用 build_request_builder，发送流式请求
        let resp = self
            .build_request_builder(&http_req)?
            .send()
            .await
            .map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.bytes().await.map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })?;
            let raw_resp = RawResponse {
                status,
                headers: HeaderMap::new(),
                body,
            };
            return Err(self.handle_error(&raw_resp));
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

            // SSE 行缓冲区 — bytes_stream() 可能截断单条 SSE 消息
            let mut sse_buffer = String::new();

            // 解析一行 SSE 字段
            fn parse_sse_line(line: &str) -> Option<(&str, &str)> {
                if let Some(pos) = line.find(':') {
                    let key = line[..pos].trim();
                    let value = line[pos + 1..].trim_start_matches(' ');
                    Some((key, value))
                } else {
                    None
                }
            }

            // 处理一个完整的 SseEvent
            async fn handle_sse_event(
                tx: &tokio::sync::mpsc::Sender<Result<ProviderEvent, LlmError>>,
                adapter: &impl ProviderAdapter,
                accumulator: &mut ToolCallAccumulator,
                event: &SseEvent,
            ) -> (bool, Option<TokenUsage>) {
                let mut is_done = false;
                let mut usage = None;

                match adapter.parse_stream_chunk(event) {
                    Ok(result) => {
                        for c in result.chunks {
                            match c {
                                StreamChunk::TextDelta(text) => {
                                    let _ = tx.send(Ok(ProviderEvent::Token { token: text })).await;
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
                    }
                }

                (is_done, usage)
            }

            while let Some(result) = boxed_stream.next().await {
                match result {
                    Ok(bytes) => {
                        let chunk_str = String::from_utf8_lossy(&bytes).to_string();
                        sse_buffer.push_str(&chunk_str);

                        // 取出完整 buffer 处理，避免借用冲突
                        let buffer = std::mem::take(&mut sse_buffer);
                        let mut lines: Vec<String> = Vec::new();
                        let mut rest = buffer.as_str();
                        while let Some(pos) = rest.find('\n') {
                            lines.push(rest[..pos].to_string());
                            rest = &rest[pos + 1..];
                        }
                        sse_buffer = rest.to_string();

                        // 按 SSE 帧解析：event: / data: / 空行
                        let mut frame_event: Option<String> = None;
                        let mut frame_data: Option<String> = None;

                        for line in lines {
                            let line_trimmed = line.trim();
                            if line_trimmed.is_empty() {
                                // 空行 = SSE 帧边界，提交事件
                                if let Some(data) = frame_data.take() {
                                    let sse_event = SseEvent {
                                        event: frame_event.take(),
                                        data,
                                    };
                                    let (done, u) = handle_sse_event(
                                        &tx,
                                        &adapter,
                                        &mut accumulator,
                                        &sse_event,
                                    )
                                    .await;
                                    if done {
                                        is_done = true;
                                    }
                                    if let Some(u) = u {
                                        usage = Some(u);
                                    }
                                }
                                continue;
                            }

                            if let Some((key, value)) = parse_sse_line(line_trimmed) {
                                match key {
                                    "event" => {
                                        frame_event = Some(value.to_string());
                                    }
                                    "data" => {
                                        if value.is_empty() {
                                            continue;
                                        }
                                        frame_data.get_or_insert_with(String::new).push_str(value);
                                    }
                                    _ => {} // id:, retry: 等忽略
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
                        return;
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

// ─── ProviderConfig — 只管连接（base_url, auth, timeout），不含 model ───

/// Provider 配置 — 只管连接，不含 model。
#[derive(Clone, Debug)]
pub struct ProviderConfig {
    /// API 基础地址
    pub base_url: url::Url,
    /// 认证配置
    pub auth: AuthConfig,
    /// 请求超时
    pub timeout: std::time::Duration,
}

impl ProviderConfig {
    /// 便捷构造 — Bearer 认证（OpenAI 等大多数 Provider）
    pub fn bearer(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::Bearer {
                api_key: secrecy::SecretString::new(api_key.into()),
            },
            timeout: std::time::Duration::from_secs(120),
        })
    }

    /// 便捷构造 — 自定义 Header 认证（Anthropic 等）
    pub fn header(
        base_url: impl AsRef<str>,
        header: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::Header {
                header: header.into(),
                value: secrecy::SecretString::new(value.into()),
            },
            timeout: std::time::Duration::from_secs(120),
        })
    }

    /// 便捷构造 — 无认证（本地调试）
    pub fn none(base_url: impl AsRef<str>) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::None,
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
    /// 将认证 header 应用到 RequestBuilder。
    /// Secret 在 header() 内部直接消费，不经过中间变量。
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            AuthConfig::Bearer { api_key } => builder.bearer_auth(api_key.expose_secret()),
            AuthConfig::Header { header, value } => builder.header(header, value.expose_secret()),
            AuthConfig::None => builder,
        }
    }
}

// ─── ToolCallAccumulator — 按 index 聚合增量 delta ───

/// ToolCall 增量组装器 — 按 index 聚合（GenericProvider 内部使用）
///
/// 以 index 为 key，因为很多 Provider 的第一批 delta 只有 index 而没有 id。
pub(crate) struct ToolCallAccumulator {
    current: std::collections::HashMap<usize, PendingToolCall>,
}

struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self {
            current: std::collections::HashMap::new(),
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
