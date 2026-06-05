//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（HTTP 发送、认证、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的协议格式转换。

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::HeaderMap;
use lellm_core::{ChatRequest, ChatResponse, LlmError, TokenUsage};
use secrecy::ExposeSecret;
use std::borrow::Cow;

use crate::{LlmProvider, ProviderEvent, ProviderStream};

use super::stream::sse_frame::SseFrame;
pub(crate) use super::stream::tool_call_accumulator::ToolCallDelta;
use super::stream::{EventSink, StreamEvent};

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
    #[allow(dead_code)]
    pub headers: HeaderMap,
    pub body: Bytes,
}

// ─── 流式解析中间表示（Adapter SPI） ───

/// 流式 chunk — Adapter 解析 SseFrame 后返回。
#[derive(Debug)]
pub(crate) enum StreamChunk {
    TextDelta(String),
    ToolCallDelta(ToolCallDelta),
    /// 完整 usage（OpenAI 最后一个 chunk 携带）
    Usage(TokenUsage),
    /// 输入 token 计数（Anthropic message_start 事件）
    InputTokens(u32),
    /// 输出 token 计数（Anthropic message_delta 事件）
    OutputTokens(u32),
    Done,
}

/// 流式解析结果 — 可能包含多个 chunk。
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

    /// 解析单个 SSE 帧的 data 字段。
    /// SSE 协议解析（缓冲、行拆分、event/data 提取）由 GenericProvider 统一处理，构建 SseFrame。
    fn parse_sse_frame(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;
}

// ─── EventSink 适配：将 tokio channel 包装为 EventSink ───

/// Channel Sink — 将 StreamEvent 转换为 ProviderEvent 并发送到 channel。
///
/// 这是 stream/ 模块与 GenericProvider 之间的唯一桥接点。
struct ChannelSink {
    tx: tokio::sync::mpsc::Sender<Result<ProviderEvent, LlmError>>,
}

impl EventSink for ChannelSink {
    async fn emit(&mut self, event: StreamEvent) {
        if event.is_critical() {
            // 关键事件（Error, ResponseComplete）必须送达
            let mapped = map_stream_event(event);
            if let Err(e) = self.tx.send(mapped).await {
                tracing::error!(
                    error = %e,
                    "critical stream event lost: channel closed"
                );
            }
        } else {
            // Token 等可丢弃事件，channel 满时丢弃
            let mapped = map_stream_event(event);
            if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) =
                self.tx.try_send(mapped)
            {
                tracing::warn!("stream event dropped: channel full");
            }
        }
    }
}

fn map_stream_event(event: StreamEvent) -> Result<ProviderEvent, LlmError> {
    match event {
        StreamEvent::Start { model } => Ok(ProviderEvent::Start { model }),
        StreamEvent::Token { token } => Ok(ProviderEvent::Token { token }),
        StreamEvent::Error(e) => Err(e),
        StreamEvent::ResponseComplete { tool_calls, usage } => {
            Ok(ProviderEvent::ResponseComplete { tool_calls, usage })
        }
    }
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

        // 发送流式请求
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

        // 将 reqwest::Response 转换为通用字节流：Stream<Item = Result<Bytes, LlmError>>
        let byte_stream = resp.bytes_stream().map(|item| {
            item.map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })
        });
        let boxed_stream = Box::pin(byte_stream);

        // 使用 ChannelSink 桥接 EventSink ←→ tokio channel
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut sink = ChannelSink { tx };
        tokio::spawn(async move {
            super::stream::stream_processor::process_stream(
                &mut sink,
                &adapter,
                model,
                boxed_stream,
            )
            .await;
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
