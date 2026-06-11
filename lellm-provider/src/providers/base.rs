//! CodecProvider — 持有 Codec + 连接配置，统一 HTTP 传输。
//!
//! CodecProvider 封装通用逻辑（HTTP 发送、认证、超时、流式解析），
//! Codec 只负责请求/响应的协议格式转换。
//!
//! 协议编解码 SPI 定义在 [`codec`] 模块中。

use async_trait::async_trait;
use http::HeaderMap;
use lellm_core::{ChatRequest, ChatResponse, LlmError};
use secrecy::ExposeSecret;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_stream::StreamExt;

use crate::{LlmProvider, ProviderEvent, ProviderStream};

use super::codec::{
    Capabilities, validate_capabilities, CodecRequest, ProviderEnvError, ProviderExtension,
    ProviderMeta,
};
use super::stream::{EventSink, StreamEvent};

// ─── HTTP 原始响应 ───

/// HTTP 原始响应 — CodecProvider 接收，4xx/5xx 由 CodecProvider 处理。
#[derive(Debug)]
pub(crate) struct RawResponse {
    pub status: u16,
    /// 响应 Header — 为 `Retry-After` 限流控制预留。
    /// v0.1 未消费，v0.2+ 用于 RateLimited 错误的智能退避。
    #[allow(dead_code)]
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
}

// ─── EventSink 适配：将 tokio channel 包装为 EventSink ───

/// Channel Sink — 将 StreamEvent 转换为 ProviderEvent 并发送到 channel。
///
/// 这是 stream/ 模块与 CodecProvider 之间的唯一桥接点。
/// `emit` 返回 `false` 表示消费者已断开。
///
/// 关键事件（Start, Error, ResponseComplete）：阻塞发送，绝不丢弃。
/// 非关键事件（Token, ThinkingDelta）：try_send，channel 满时丢弃并计数。
struct ChannelSink {
    tx: tokio::sync::mpsc::Sender<Result<ProviderEvent, LlmError>>,
    dropped: Arc<AtomicU64>,
}

impl EventSink for ChannelSink {
    async fn emit(&mut self, event: StreamEvent) -> bool {
        if event.is_critical() {
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
            let mapped = map_stream_event(event);
            match self.tx.try_send(mapped) {
                Ok(()) => true,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
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

// ─── CodecProvider — 持有 codec + config + client ───

/// 通用 Provider，适配任何 ProviderExtension。
///
/// Codec 通过 `Arc<C>` 持有，支持零开销共享（无状态 Codec）
/// 或共享状态（如调用计数器、tokenize 缓存）。
/// 不需要 `Clone` bound。
#[allow(private_bounds)]
pub struct CodecProvider<C: ProviderExtension> {
    codec: Arc<C>,
    config: ProviderConfig,
    client: reqwest::Client,
    /// Builder 传入的额外 Headers，用于覆盖 codec defaults。
    /// 合并优先级：codec default_headers → extra_headers → CodecRequest headers。
    extra_headers: HeaderMap,
}

#[allow(private_bounds)]
impl<C: ProviderExtension> CodecProvider<C> {
    /// 创建 CodecProvider。
    ///
    /// 使用 [`ProviderBuilder`] 进行更灵活的构建（支持自定义 Headers 等）。
    ///
    /// ```rust,no_run
    /// use lellm_provider::{CodecProvider, ProviderBuilder, OpenAICompatCodec};
    ///
    /// let provider = CodecProvider::builder(OpenAICompatCodec::openai())
    ///     .api_key(std::env::var("OPENAI_API_KEY").unwrap())
    ///     .build()?;
    /// # Ok::<_, url::ParseError>(())
    /// ```
    pub fn new(codec: C, config: ProviderConfig) -> Self {
        Self::builder(codec)
            .base_url(config.base_url.as_str())
            .auth(config.auth)
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .idle_timeout(config.idle_timeout)
            .build()
            .expect("ProviderConfig base_url was already validated")
    }

    /// 从环境变量自动加载配置创建 Provider（便捷方法）。
    ///
    /// 内部委托给 `ProviderConfig::from_codec(&codec)`。
    ///
    /// # 示例
    ///
    /// ```rust,no_run
    /// use lellm_provider::{CodecProvider, OpenAICompatCodec};
    ///
    /// let provider = CodecProvider::from_env(OpenAICompatCodec::openai())?;
    /// # Ok::<_, lellm_provider::providers::codec::ProviderEnvError>(())
    /// ```
    pub fn from_env(codec: C) -> Result<Self, ProviderEnvError> {
        let config = ProviderConfig::from_codec(&codec)?;
        Ok(Self::new(codec, config))
    }

    /// 创建 ProviderBuilder，支持链式配置。
    ///
    /// Builder 允许设置自定义 Headers、覆盖 base_url、调整超时等。
    ///
    /// ```rust,no_run
    /// use lellm_provider::{CodecProvider, OpenAICompatCodec};
    ///
    /// let provider = CodecProvider::builder(OpenAICompatCodec::openai())
    ///     .base_url("https://openrouter.ai/api/v1")
    ///     .api_key("sk-or-...")
    ///     .header("HTTP-Referer", "https://example.com")
    ///     .header("X-Title", "My App")
    ///     .build()?;
    /// # Ok::<_, url::ParseError>(())
    /// ```
    pub fn builder(codec: C) -> ProviderBuilder<C> {
        ProviderBuilder::new(codec)
    }

    /// 校验 ChatRequest：消息语义 + Provider 能力匹配。
    fn validate_request(&self, req: &ChatRequest) -> Result<(), LlmError> {
        for msg in &req.messages {
            msg.validate()
                .map_err(|e| LlmError::Parse { detail: e.detail })?;
        }

        let caps = self.codec.capabilities_for(&req.model);
        validate_capabilities(req, &caps)?;

        Ok(())
    }

    fn build_request_builder(
        &self,
        req: &CodecRequest,
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

        // 三层 Header 合并：codec defaults → builder extra_headers → request headers
        // 后者覆盖前者
        let mut merged = self.codec.default_headers().clone();
        merged.extend(self.extra_headers.clone());
        merged.extend(req.headers.clone());

        let builder = merged
            .iter()
            .fold(builder, |b, (key, value)| b.header(key, value));

        Ok(builder.body(req.body.clone()))
    }

    async fn send(&self, req: CodecRequest) -> Result<RawResponse, LlmError> {
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

    fn handle_error(&self, resp: &RawResponse) -> LlmError {
        let body_str = String::from_utf8_lossy(&resp.body);
        match resp.status {
            401 => LlmError::Provider {
                provider: self.codec.provider_id().to_string(),
                status: Some(401),
                code: None,
                message: body_str.into_owned(),
            },
            429 => LlmError::Provider {
                provider: self.codec.provider_id().to_string(),
                status: Some(429),
                code: None,
                message: "rate limited".into(),
            },
            status @ (400..=599) => LlmError::Provider {
                provider: self.codec.provider_id().to_string(),
                status: Some(status),
                code: None,
                message: body_str.into_owned(),
            },
            _ => LlmError::Provider {
                provider: self.codec.provider_id().to_string(),
                status: Some(resp.status),
                code: None,
                message: format!("Unexpected status: {}", resp.status),
            },
        }
    }
}

#[async_trait]
#[allow(private_bounds)]
impl<C: ProviderExtension + 'static> LlmProvider for CodecProvider<C> {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        self.validate_request(request)?;
        let http_req = self.codec.encode(request, false)?;

        let resp = match tokio::time::timeout(self.config.timeout, self.send(http_req)).await {
            Ok(result) => result?,
            Err(_elapsed) => {
                return Err(LlmError::Timeout {
                    detail: format!("request timed out after {:?}", self.config.timeout),
                });
            }
        };

        if (200..=299).contains(&resp.status) {
            self.codec.decode(&resp.body)
        } else {
            Err(self.handle_error(&resp))
        }
    }

    async fn stream(
        &self,
        request: &ChatRequest,
    ) -> Result<ProviderStream, LlmError> {
        self.validate_request(request)?;
        let http_req = self.codec.encode(request, true)?;

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

        let byte_stream = resp.bytes_stream();
        let mut first_token_guard = byte_stream;
        let first_chunk = match tokio::time::timeout(
            self.config.timeout,
            tokio_stream::StreamExt::next(&mut first_token_guard),
        )
        .await
        {
            Ok(Some(result)) => result,
            Ok(None) => return Err(LlmError::UnexpectedEof),
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
        let codec = Arc::clone(&self.codec);

        let idle_timeout = self.config.idle_timeout;
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

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_clone = Arc::clone(&dropped);
        let mut sink = ChannelSink { tx, dropped };
        tokio::spawn(async move {
            super::stream::stream_processor::process_stream(
                &mut sink,
                &*codec,
                model,
                boxed_stream,
            )
            .await;
            // 流式处理结束后，报告丢弃的事件数
            let n = dropped_clone.load(Ordering::Relaxed);
            if n > 0 {
                tracing::warn!(
                    dropped_events = n,
                    "non-critical stream events were dropped due to full channel"
                );
            }
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        Ok(Box::pin(rx_stream))
    }

    fn provider_id(&self) -> &str {
        self.codec.provider_id()
    }

    fn capabilities_for(&self, model: &str) -> Capabilities {
        self.codec.capabilities_for(model)
    }
}

// ─── ProviderConfig — 只管连接（base_url, auth, timeout）──

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
    pub idle_timeout: std::time::Duration,
}

impl ProviderConfig {
    /// 便捷构造 — Bearer 认证
    pub fn bearer(
        base_url: impl AsRef<str>,
        api_key: impl Into<String>,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            base_url: url::Url::parse(base_url.as_ref())?,
            auth: AuthConfig::Bearer {
                api_key: secrecy::SecretString::new(api_key.into()),
            },
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(120),
            idle_timeout: std::time::Duration::from_secs(30),
        })
    }

    /// 便捷构造 — 自定义 Header 认证
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

    /// 从 codec 元数据 + 环境变量自动加载配置。
    pub fn from_codec(meta: &dyn ProviderMeta) -> Result<Self, ProviderEnvError> {
        let provider_id = meta.provider_id();
        let env_prefix = provider_id.to_ascii_uppercase();
        let default_url = meta.default_base_url();
        let auth_style = meta.auth_style();
        let api_key_env = meta.api_key_env();

        let base_url = std::env::var(format!("{}_BASE_URL", env_prefix)).unwrap_or_else(|_| {
            tracing::debug!(
                provider = provider_id,
                url = default_url,
                "{}_BASE_URL not set, using default",
                env_prefix
            );
            default_url.to_string()
        });

        let api_key =
            std::env::var(&*api_key_env).map_err(|_| ProviderEnvError::MissingApiKey {
                provider: provider_id.to_string(),
                env_var: api_key_env.into_owned(),
            })?;

        match auth_style {
            super::codec::AuthStyle::Bearer => {
                Self::bearer(&base_url, api_key).map_err(|e| ProviderEnvError::InvalidUrl {
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            }
            super::codec::AuthStyle::CustomHeader(header) => {
                Self::header(&base_url, header, api_key).map_err(|e| ProviderEnvError::InvalidUrl {
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            }
            super::codec::AuthStyle::None => {
                Self::none(&base_url).map_err(|e| ProviderEnvError::InvalidUrl {
                    url: base_url.clone(),
                    reason: e.to_string(),
                })
            }
        }
    }

    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_connect_timeout(mut self, connect_timeout: std::time::Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

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
    Bearer {
        api_key: secrecy::SecretString,
    },
    Header {
        header: String,
        value: secrecy::SecretString,
    },
    None,
}

impl AuthConfig {
    pub fn apply(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            AuthConfig::Bearer { api_key } => builder.bearer_auth(api_key.expose_secret()),
            AuthConfig::Header { header, value } => builder.header(header, value.expose_secret()),
            AuthConfig::None => builder,
        }
    }
}

// ─── ProviderBuilder — 链式构建 CodecProvider ───

/// CodecProvider 的链式构建器。
///
/// 支持自定义 base_url、认证、超时、额外 Headers。
/// 额外 Headers 在发送请求时与 codec defaults 及 request headers 合并：
/// codec default_headers → builder extra_headers → CodecRequest headers（后者覆盖前者）。
///
/// # 示例：OpenRouter
///
/// ```rust,no_run
/// use lellm_provider::{CodecProvider, OpenAICompatCodec};
///
/// let provider = CodecProvider::builder(OpenAICompatCodec::openai())
///     .base_url("https://openrouter.ai/api/v1")
///     .api_key("sk-or-...")
///     .header("HTTP-Referer", "https://mysite.com")
///     .header("X-Title", "My App")
///     .build()?;
/// # Ok::<_, url::ParseError>(())
/// ```
pub struct ProviderBuilder<C> {
    codec: C,
    base_url: String,
    auth: Option<AuthConfig>,
    connect_timeout: std::time::Duration,
    timeout: std::time::Duration,
    idle_timeout: std::time::Duration,
    extra_headers: HeaderMap,
}

impl<C> ProviderBuilder<C> {
    /// 创建新的 Builder，仅持有 Codec。
    fn new(codec: C) -> Self {
        Self {
            codec,
            base_url: String::new(),
            auth: None,
            connect_timeout: std::time::Duration::from_secs(10),
            timeout: std::time::Duration::from_secs(120),
            idle_timeout: std::time::Duration::from_secs(30),
            extra_headers: HeaderMap::new(),
        }
    }

    /// 设置 API 基础地址。
    pub fn base_url(mut self, url: impl AsRef<str>) -> Self {
        self.base_url = url.as_ref().to_string();
        self
    }

    /// 设置 Bearer Token 认证（`Authorization: Bearer <key>`）。
    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.auth = Some(AuthConfig::Bearer {
            api_key: secrecy::SecretString::new(key.into()),
        });
        self
    }

    /// 设置自定义 Header 认证。
    pub fn auth_header(
        mut self,
        header: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.auth = Some(AuthConfig::Header {
            header: header.into(),
            value: secrecy::SecretString::new(value.into()),
        });
        self
    }

    /// 设置完整的认证配置。
    pub fn auth(mut self, auth: AuthConfig) -> Self {
        self.auth = Some(auth);
        self
    }

    /// 添加一个自定义 Header。可链式调用多次。
    ///
    /// 用于注入 Provider 要求的额外 Headers，如 OpenRouter 的 `HTTP-Referer`。
    pub fn header(mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        let name: http::HeaderName = key.as_ref().parse().expect("invalid header name");
        let val: http::HeaderValue = value.as_ref().parse().expect("invalid header value");
        self.extra_headers.insert(name, val);
        self
    }

    /// 批量添加自定义 Headers。
    pub fn extra_headers(
        mut self,
        headers: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        for (key, value) in headers {
            let name: http::HeaderName = key.parse().expect("invalid header name");
            let val: http::HeaderValue = value.parse().expect("invalid header value");
            self.extra_headers.insert(name, val);
        }
        self
    }

    /// 设置 TCP/TLS 握手超时。
    pub fn connect_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// 设置请求超时（非流式请求 + 流式首 token 等待）。
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 设置 SSE 流空闲超时。
    pub fn idle_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// 构建 CodecProvider。
    pub fn build(self) -> Result<CodecProvider<C>, url::ParseError>
    where
        C: ProviderExtension,
    {
        let base_url = url::Url::parse(&self.base_url)?;
        let auth = self.auth.unwrap_or(AuthConfig::None);

        // Network-level idle guard: significantly larger than stream idle timeout.
        let network_idle_guard = std::cmp::max(
            self.idle_timeout.saturating_mul(3),
            std::time::Duration::from_secs(120),
        );
        let client = reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .read_timeout(network_idle_guard)
            .user_agent(format!("LeLLM/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();

        Ok(CodecProvider {
            codec: Arc::new(self.codec),
            config: ProviderConfig {
                base_url,
                auth,
                connect_timeout: self.connect_timeout,
                timeout: self.timeout,
                idle_timeout: self.idle_timeout,
            },
            client,
            extra_headers: self.extra_headers,
        })
    }
}

/// 便捷构造 OpenRouter Provider。
///
/// OpenRouter 是一个聚合网关，支持多种协议（OpenAI / Anthropic / Responses）。
/// 此函数设置正确的 base_url 和 Bearer 认证，返回 CodecProvider。
///
/// **环境变量回退**：优先读取 `OPENROUTER_API_KEY`，未设置则返回错误。
/// `OPENROUTER_BASE_URL` 可选，默认 `https://openrouter.ai/api/v1`。
///
/// # 示例
///
/// ```rust,no_run
/// use lellm_provider::{openrouter, OpenAICompatCodec};
///
/// // 从 OPENROUTER_API_KEY 环境变量加载
/// let provider = openrouter(OpenAICompatCodec::openai())?;
/// # Ok::<_, lellm_provider::providers::codec::ProviderEnvError>(())
/// ```
///
/// 如需添加 `HTTP-Referer` 或 `X-Title` 等推荐 Header，使用 [`ProviderBuilder`]：
///
/// ```rust,no_run
/// use lellm_provider::{CodecProvider, OpenAICompatCodec};
///
/// let provider = CodecProvider::builder(OpenAICompatCodec::openai())
///     .base_url("https://openrouter.ai/api/v1")
///     .api_key("sk-or-...")
///     .header("HTTP-Referer", "https://mysite.com")
///     .header("X-Title", "My App")
///     .build()?;
/// # Ok::<_, url::ParseError>(())
/// ```
pub fn openrouter<C: ProviderExtension>(codec: C) -> Result<CodecProvider<C>, ProviderEnvError> {
    let base_url = std::env::var("OPENROUTER_BASE_URL").unwrap_or_else(|_| {
        tracing::debug!("OPENROUTER_BASE_URL not set, using default");
        "https://openrouter.ai/api/v1".to_string()
    });

    let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| ProviderEnvError::MissingApiKey {
        provider: "openrouter".to_string(),
        env_var: "OPENROUTER_API_KEY".to_string(),
    })?;

    Ok(CodecProvider::builder(codec)
        .base_url(&base_url)
        .api_key(api_key)
        .build()
        .map_err(|e| ProviderEnvError::InvalidUrl {
            url: base_url.clone(),
            reason: e.to_string(),
        })?)
}
