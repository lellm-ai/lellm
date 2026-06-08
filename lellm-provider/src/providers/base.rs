//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（HTTP 发送、认证、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的协议格式转换。

use async_trait::async_trait;
use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{ChatRequest, ChatResponse, ContentBlock, LlmError, TokenUsage};
use secrecy::ExposeSecret;
use std::borrow::Cow;
use tokio_stream::StreamExt;

use crate::{LlmProvider, ProviderEvent, ProviderStream};

use super::stream::sse_frame::SseFrame;
pub(crate) use super::stream::tool_call_accumulator::ToolCallDelta;
use super::stream::{EventSink, StreamEvent};

// ─── Provider 认证风格与错误 ───

/// 认证方式
#[derive(Debug, Clone, Copy)]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// 自定义 header，e.g. `x-api-key: <key>`
    CustomHeader(&'static str),
    /// 无认证
    None,
}

/// 环境变量加载错误
#[derive(Debug)]
pub enum ProviderEnvError {
    /// 缺少必需的 API Key
    MissingApiKey { provider: String },
    /// URL 解析失败
    InvalidUrl { url: String, reason: String },
}

impl std::fmt::Display for ProviderEnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderEnvError::MissingApiKey { provider } => {
                write!(f, "Missing API key for provider '{}'", provider)
            }
            ProviderEnvError::InvalidUrl { url, reason } => {
                write!(f, "Invalid URL '{}': {}", url, reason)
            }
        }
    }
}

impl std::error::Error for ProviderEnvError {}

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
    /// 思考块增量（Anthropic thinking_delta / OpenAI reasoning_content）
    ThinkingDelta {
        thinking: String,
        redacted: Option<String>,
    },
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
    /// Provider 标识（全小写，如 "openai", "anthropic"）。
    /// 环境变量前缀自动推导为 `provider_id().to_ascii_uppercase()`。
    fn provider_id(&self) -> &str;

    /// 默认基础 URL（当 `<PREFIX>_BASE_URL` 未设置时使用）。
    fn default_base_url(&self) -> &'static str;

    /// 认证方式（决定使用 `bearer()` 还是 `header()` 构造配置）。
    fn auth_style(&self) -> AuthStyle;

    /// 构建 Provider 请求（路径 + 协议 Header + JSON Body 字节）
    fn build_request(&self, req: &ChatRequest, stream: bool) -> Result<ProviderRequest, LlmError>;

    /// 解析成功响应 body（2xx）为 ChatResponse
    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;

    /// 解析单个 SSE 帧的 data 字段。
    /// SSE 协议解析（缓冲、行拆分、event/data 提取）由 GenericProvider 统一处理，构建 SseFrame。
    fn parse_sse_frame(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;

    /// Provider 能力声明。
    /// 默认不支持图片输入。Adapter 可覆写以声明支持。
    fn supports_image_input(&self) -> bool {
        false
    }
}

// ─── EventSink 适配：将 tokio channel 包装为 EventSink ───

/// Channel Sink — 将 StreamEvent 转换为 ProviderEvent 并发送到 channel。
///
/// 这是 stream/ 模块与 GenericProvider 之间的唯一桥接点。
/// `emit` 返回 `false` 表示消费者已断开。
struct ChannelSink {
    tx: tokio::sync::mpsc::Sender<Result<ProviderEvent, LlmError>>,
}

impl EventSink for ChannelSink {
    async fn emit(&mut self, event: StreamEvent) -> bool {
        if event.is_critical() {
            // 关键事件（Error, ResponseComplete）必须送达
            let mapped = map_stream_event(event);
            match self.tx.send(mapped).await {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "critical stream event lost: channel closed"
                    );
                    false
                }
            }
        } else {
            // Token 等可丢弃事件，channel 满时丢弃
            let mapped = map_stream_event(event);
            match self.tx.try_send(mapped) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // channel 满时静默丢弃 Token 事件（预期行为）
                    true // channel 没断，只是满
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    false // channel 断了
                }
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

fn map_stream_event(event: StreamEvent) -> Result<ProviderEvent, LlmError> {
    match event {
        StreamEvent::Start { model } => Ok(ProviderEvent::Start { model }),
        StreamEvent::Token { token } => Ok(ProviderEvent::Token { token }),
        StreamEvent::ThinkingDelta { thinking, redacted } => {
            Ok(ProviderEvent::ThinkingDelta { thinking, redacted })
        }
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
            // connect_timeout — 仅限制 TCP/TLS 握手时间
            .connect_timeout(config.connect_timeout)
            // read_timeout — 作为 idle_timeout 的后置防线（取其 2 倍，给 TCP 层余量）
            .read_timeout(config.idle_timeout.saturating_mul(2))
            .user_agent(format!("LeLLM/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();

        Self {
            adapter,
            config,
            client,
        }
    }

    /// 从环境变量自动加载配置创建 Provider（便捷方法）。
    ///
    /// 内部委托给 `ProviderConfig::from_adapter(&adapter)`。
    /// 如果需要自定义超时等配置，使用 `ProviderConfig::from_adapter` + `GenericProvider::new`。
    ///
    /// # 环境变量
    ///
    /// 前缀 = `adapter.provider_id().to_ascii_uppercase()`
    /// 读取 `<PREFIX>_BASE_URL`（可选）和 `<PREFIX>_API_KEY`（必需）。
    ///
    /// # 示例
    ///
    /// ```rust,no_run
    /// use lellm_provider::{GenericProvider, OpenAICompatAdapter};
    ///
    /// // 一行搞定：
    /// let provider = GenericProvider::from_env(OpenAICompatAdapter::openai())?;
    /// # Ok::<_, lellm_provider::providers::base::ProviderEnvError>(())
    /// ```
    pub fn from_env(adapter: A) -> Result<Self, ProviderEnvError> {
        let config = ProviderConfig::from_adapter(&adapter)?;
        Ok(Self::new(adapter, config))
    }

    /// 校验 ChatRequest：消息语义 + Provider 能力匹配。
    /// 在 build_request 之前调用，拦截非法请求。
    fn validate_request(&self, req: &ChatRequest) -> Result<(), LlmError> {
        // 1. 消息语义校验
        for msg in &req.messages {
            msg.validate()
                .map_err(|e| LlmError::Parse { detail: e.detail })?;
        }

        // 2. Provider 能力校验 — Image 输入（检查所有消息变体）
        if !self.adapter.supports_image_input() {
            for msg in &req.messages {
                for block in msg.content() {
                    if let ContentBlock::Image { .. } = block {
                        return Err(LlmError::UnsupportedFeature {
                            feature: format!(
                                "Image input ({} adapter)",
                                self.adapter.provider_id()
                            ),
                        });
                    }
                }
            }
        }

        Ok(())
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
            401 => LlmError::Provider {
                provider: self.adapter.provider_id().to_string(),
                status: Some(401),
                code: None,
                message: body_str.into_owned(),
            },
            429 => LlmError::Provider {
                provider: self.adapter.provider_id().to_string(),
                status: Some(429),
                code: None,
                message: "rate limited".into(),
            },
            status @ (400..=599) => LlmError::Provider {
                provider: self.adapter.provider_id().to_string(),
                status: Some(status),
                code: None,
                message: body_str.into_owned(),
            },
            _ => LlmError::Provider {
                provider: self.adapter.provider_id().to_string(),
                status: Some(resp.status),
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
        self.validate_request(request)?;
        let http_req = self.adapter.build_request(request, false)?;

        // 非流式：tokio::time::timeout 控制整体请求超时（防止模型卡死 / provider 挂死）
        let resp = match tokio::time::timeout(self.config.timeout, self.send(http_req)).await {
            Ok(result) => result?,
            Err(_elapsed) => {
                return Err(LlmError::Timeout {
                    detail: format!("request timed out after {:?}", self.config.timeout),
                });
            }
        };

        if (200..=299).contains(&resp.status) {
            self.adapter.parse_response(&resp.body)
        } else {
            Err(self.handle_error(&resp))
        }
    }

    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        self.validate_request(request)?;
        let http_req = self.adapter.build_request(request, true)?;

        // 发送流式请求 — 无整体超时（connect_timeout 已在 ClientBuilder 设置）
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

        // 首 token 超时 — 防止连接建立后长时间无响应
        let byte_stream = resp.bytes_stream();
        let mut first_token_guard = byte_stream;
        let first_chunk = match tokio::time::timeout(
            self.config.timeout,
            tokio_stream::StreamExt::next(&mut first_token_guard),
        )
        .await
        {
            Ok(Some(result)) => result,
            Ok(None) => {
                return Err(LlmError::UnexpectedEof);
            }
            Err(_elapsed) => {
                return Err(LlmError::Timeout {
                    detail: format!("first token timed out after {:?}", self.config.timeout),
                });
            }
        };

        let first_bytes = first_chunk.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        let model = request.model.clone();
        let stream_thinking = request.stream_thinking;
        let adapter = self.adapter.clone();

        // 将首 chunk + 剩余流拼接为通用字节流
        // timeout() 对每个 chunk 施加 idle_timeout，防止代理中途卡死
        let idle_timeout = self.config.idle_timeout; // Duration is Copy
        let remaining = first_token_guard
            .map(move |item| {
                item.map_err(|e| LlmError::Network {
                    detail: e.to_string(),
                })
            })
            .timeout(idle_timeout)
            .map(move |item| match item {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(e)) => Err(e),
                Err(_elapsed) => {
                    tracing::error!(?idle_timeout, "stream idle timeout triggered");
                    Err(LlmError::Timeout {
                        detail: format!("stream idle timed out after {:?}", idle_timeout),
                    })
                }
            });
        let byte_stream =
            futures_util::stream::once(async move { Ok(first_bytes) }).chain(remaining);
        let boxed_stream = Box::pin(byte_stream);

        // 使用 ChannelSink 桥接 EventSink ←→ tokio channel
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let mut sink = ChannelSink { tx };
        tokio::spawn(async move {
            super::stream::stream_processor::process_stream(
                &mut sink,
                &adapter,
                model,
                stream_thinking,
                boxed_stream,
            )
            .await;
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        Ok(Box::pin(rx_stream))
    }

    fn provider_id(&self) -> &str {
        self.adapter.provider_id()
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
    /// TCP/TLS 握手超时
    pub connect_timeout: std::time::Duration,
    /// 请求超时 — 控制 `.send()` + 首 token 等待
    pub timeout: std::time::Duration,
    /// SSE 流空闲超时 — 连续无数据的最大时间（per-chunk）。
    /// 防止代理或上游中途卡死导致无限等待。
    pub idle_timeout: std::time::Duration,
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
                api_key: secrecy::SecretString::new(api_key.into().into()),
            },
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(120),
            idle_timeout: std::time::Duration::from_secs(30),
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
                value: secrecy::SecretString::new(value.into().into()),
            },
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(120),
            idle_timeout: std::time::Duration::from_secs(30),
        })
    }

    /// 便捷构造 — 无认证（本地调试）
    pub fn none(base_url: impl AsRef<str>) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::None,
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(120),
            idle_timeout: std::time::Duration::from_secs(30),
        })
    }

    /// 从 adapter 元数据 + 环境变量自动加载配置。
    ///
    /// 环境变量前缀 = `adapter.provider_id().to_ascii_uppercase()`。
    /// 读取 `<PREFIX>_BASE_URL`（可选，有默认值）和 `<PREFIX>_API_KEY`（必需）。
    ///
    /// 返回 `ProviderConfig`，可链式修改超时后再传给 `GenericProvider::new()`。
    ///
    /// # 示例
    ///
    /// ```rust,no_run
    /// use lellm_provider::{GenericProvider, OpenAICompatAdapter};
    /// use lellm_provider::providers::base::ProviderConfig;
    ///
    /// let adapter = OpenAICompatAdapter::openai();
    /// let provider = GenericProvider::new(
    ///     adapter.clone(),
    ///     ProviderConfig::from_adapter(&adapter)?
    ///         .with_timeout(std::time::Duration::from_secs(60))
    ///         .with_idle_timeout(std::time::Duration::from_secs(30)),
    /// );
    /// # Ok::<_, lellm_provider::providers::base::ProviderEnvError>(())
    /// ```
    pub(crate) fn from_adapter(adapter: &dyn ProviderAdapter) -> Result<Self, ProviderEnvError> {
        let provider_id = adapter.provider_id();
        let env_prefix = provider_id.to_ascii_uppercase();
        let default_url = adapter.default_base_url();
        let auth_style = adapter.auth_style();

        let base_url = std::env::var(format!("{}_BASE_URL", env_prefix)).unwrap_or_else(|_| {
            tracing::debug!(
                provider = provider_id,
                url = default_url,
                "{}_BASE_URL not set, using default",
                env_prefix
            );
            default_url.to_string()
        });

        let api_key = std::env::var(format!("{}_API_KEY", env_prefix)).map_err(|_| {
            tracing::error!(provider = provider_id, "{}_API_KEY not found", env_prefix);
            ProviderEnvError::MissingApiKey {
                provider: provider_id.to_string(),
            }
        })?;

        match auth_style {
            AuthStyle::Bearer => {
                Self::bearer(&base_url, api_key).map_err(|e| ProviderEnvError::InvalidUrl {
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            }
            AuthStyle::CustomHeader(header) => {
                Self::header(&base_url, header, api_key).map_err(|e| ProviderEnvError::InvalidUrl {
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            }
            AuthStyle::None => Self::none(&base_url).map_err(|e| ProviderEnvError::InvalidUrl {
                url: base_url.clone(),
                reason: e.to_string(),
            }),
        }
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

    /// 修改连接超时
    pub fn with_connect_timeout(mut self, connect_timeout: std::time::Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// 修改 SSE 流空闲超时
    pub fn with_idle_timeout(mut self, idle_timeout: std::time::Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: url::Url::parse("http://localhost").unwrap(),
            auth: AuthConfig::None,
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(120),
            idle_timeout: std::time::Duration::from_secs(30),
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
