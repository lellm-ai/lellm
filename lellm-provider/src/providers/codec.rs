//! ProviderCodec — 三权分立的协议编解码 SPI。
//!
//! 将 Provider 的职责拆分为三个独立 trait：
//! - **ChatCodec** — 协议编解码（encode/decode/decode_sse），无状态物理层互转
//! - **ModelCapabilities** — 模型感知能力矩阵，逻辑校验层
//! - **ProviderMeta** — 连接元数据（provider_id/base_url/auth），框架环境约定
//!
//! 生态扩展统一入口：`ProviderExtension: ChatCodec + ModelCapabilities + ProviderMeta`
//! 开发者只需实现 `ProviderExtension`，框架内部按需消费子 trait。
//!
//! `CodecProvider` 持有 Codec + 连接配置，统一负责 HTTP 发送、认证注入、超时控制。

use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{ChatRequest, ChatResponse, LlmError};
use std::borrow::Cow;

use super::stream::sse_frame::SseFrame;
pub use super::stream::tool_call_accumulator::ToolCallDelta;

// ─── Codec 编解码中间表示 ───

/// Codec 请求 — Codec 构建，CodecProvider 发送。
///
/// Codec 只关心协议适配（路径、Header、Body），
/// 不关心 base_url、认证、HTTP Client。
#[derive(Debug)]
pub struct CodecRequest {
    /// 相对路径。例如 `/v1/chat/completions` 或 `/v1beta/models/gemini-pro:generateContent`。
    pub path: Cow<'static, str>,
    /// 该厂商特有的自定义 Headers。例如 Anthropic 的 `anthropic-version: 2023-06-01`。
    pub headers: HeaderMap,
    /// 序列化后的请求体。
    pub body: Bytes,
}

/// 流式 chunk — Codec 解析 SseFrame 后返回。
#[derive(Debug)]
pub enum StreamChunk {
    TextDelta(String),
    /// 思考块增量（Anthropic thinking_delta / OpenAI reasoning_content）
    ThinkingDelta {
        thinking: String,
        redacted: Option<String>,
    },
    ToolCallDelta(ToolCallDelta),
    /// 完整 usage（OpenAI 最后一个 chunk 携带）
    Usage(lellm_core::TokenUsage),
    /// 输入 token 计数（Anthropic message_start 事件）
    InputTokens(u32),
    /// 输出 token 计数（Anthropic message_delta 事件）
    OutputTokens(u32),
    Done,
}

/// 流式解析结果 — 可能包含多个 chunk。
#[derive(Debug)]
pub struct StreamParseResult {
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

// ─── 能力矩阵 ───

/// Provider 能力声明 — 模型感知。
///
/// 不同模型支持不同能力，Codec 通过 `capabilities_for(model)` 返回。
/// 框架在 `validate_request` 时检查能力匹配。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// 支持图片输入（User 消息中的 Image ContentBlock）
    pub supports_image_input: bool,
    /// 支持推理控制（ReasoningConfig 的 Low/Medium/High）
    pub supports_reasoning: bool,
    /// 支持工具调用（Function Calling / Tool Use）
    pub supports_tool_call: bool,
    /// 支持预填充文本（引导模型输出方向）。
    ///
    /// ⚠️ TODO: 目前没有任何 Codec 真正实现 prefill 编码。
    /// 所有 Codec 默认返回 `false`。实现后恢复 `validate_capabilities` 中的校验。
    #[allow(dead_code)]
    pub supports_prefill: bool,
    /// 支持流式输出推理过程（ThinkingDelta 事件）
    pub supports_stream_thinking: bool,
}

/// 校验请求与模型能力的匹配。
///
/// 统一入口：如果请求了某项能力但模型不支持，返回 `UnsupportedFeature`。
/// 设计原则：`Disabled` 对任何 Provider 都是"静默成功"。
/// 只有"请求了能力但 Provider 没有"才报错。
pub fn validate_capabilities(req: &ChatRequest, caps: &Capabilities) -> Result<(), LlmError> {
    // 校验图片输入
    if !caps.supports_image_input {
        for msg in &req.messages {
            for block in msg.content() {
                if let lellm_core::ContentBlock::Image { .. } = block {
                    return Err(LlmError::UnsupportedFeature {
                        feature: "image input".into(),
                    });
                }
            }
        }
    }

    // 校验推理控制 — Disabled 静默成功
    if !caps.supports_reasoning && req.reasoning.as_ref().is_some_and(|r| !r.is_disabled()) {
        return Err(LlmError::UnsupportedFeature {
            feature: "reasoning".into(),
        });
    }

    // 校验工具调用
    if req.tools.is_some() && !caps.supports_tool_call {
        return Err(LlmError::UnsupportedFeature {
            feature: "tool call".into(),
        });
    }

    // NOTE: prefill 校验暂时跳过 — 等 Provider 真正实现 prefill 编码时再恢复

    Ok(())
}

// ─── Provider 认证风格 ───

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

/// 环境变量加载错误 — 稳定语义，不公开内部结构细节。
///
/// 通过 `ProviderBuildError::Env` 间接暴露。
/// `#[non_exhaustive]` 保证未来可扩展而不破坏调用方。
#[derive(Debug)]
#[non_exhaustive]
pub enum ProviderEnvError {
    /// 缺少必需的环境变量
    MissingEnv { name: String },
    /// 环境变量存在但为空
    EmptyEnv { name: String },
}

impl std::fmt::Display for ProviderEnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEnv { name } => write!(f, "missing environment variable: {}", name),
            Self::EmptyEnv { name } => write!(f, "environment variable is empty: {}", name),
        }
    }
}

impl std::error::Error for ProviderEnvError {}

/// Provider 构建错误 — 统一所有创建路径的错误面。
///
/// `load()` / `openrouter()` / `builder().build()` 统一返回此类型。
/// `#[non_exhaustive]` 保证未来可扩展而不破坏调用方。
#[derive(Debug)]
#[non_exhaustive]
pub enum ProviderBuildError {
    /// 环境变量加载错误
    Env(ProviderEnvError),
    /// URL 解析失败
    Url(url::ParseError),
    /// Header 解析失败
    InvalidHeader { field: String, value: String },
    /// 校验失败（超时、profile 等）
    Validation { message: String },
}

impl std::fmt::Display for ProviderBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Env(e) => write!(f, "{}", e),
            Self::Url(e) => write!(f, "{}", e),
            Self::InvalidHeader { field, value } => {
                write!(f, "invalid header {}: {}", field, value)
            }
            Self::Validation { message } => write!(f, "validation error: {}", message),
        }
    }
}

impl std::error::Error for ProviderBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Env(e) => Some(e),
            Self::Url(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ProviderEnvError> for ProviderBuildError {
    fn from(err: ProviderEnvError) -> Self {
        Self::Env(err)
    }
}

impl From<url::ParseError> for ProviderBuildError {
    fn from(err: url::ParseError) -> Self {
        Self::Url(err)
    }
}

/// 构建结果类型别名
pub type BuildResult<T> = Result<T, ProviderBuildError>;

// ─── 1. ChatCodec — 协议编解码（无状态、纯粹的物理层互转）──

/// 协议编解码 trait — 无状态、纯粹的物理层互转。
///
/// Codec **不知道** `CodecProvider`、`reqwest`、HTTP。
/// 只负责：`ChatRequest → CodecRequest`（编码），`body bytes → ChatResponse`（解码）。
pub trait ChatCodec: Send + Sync {
    /// 编码 ChatRequest 为 CodecRequest（路径 + 协议 Header + JSON Body 字节）。
    fn encode(&self, req: &ChatRequest, stream: bool) -> Result<CodecRequest, LlmError>;

    /// 解码成功响应 body（2xx）为 ChatResponse。
    fn decode(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;

    /// 解码单个 SSE 帧的 data 字段。
    /// SSE 协议解析（缓冲、行拆分、event/data 提取）由 CodecProvider 统一处理，构建 SseFrame。
    fn decode_sse(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;
}

// ─── 2. ModelCapabilities — 能力声明（模型感知、逻辑校验层）──

/// 模型感知能力矩阵 — 逻辑校验层。
///
/// 框架提供默认的基于命名的启发式匹配，Codec 可 override 精确控制。
pub trait ModelCapabilities: Send + Sync {
    /// 结合模型名称，返回该模型的能力矩阵。
    ///
    /// 默认实现返回全 false（最保守假设）。
    /// Codec 应 override 此方法以提供精确的能力声明。
    fn capabilities_for(&self, _model: &str) -> Capabilities {
        Capabilities::default()
    }
}

// NOTE: heuristic_guess 已移除。Codec 应实现 capabilities_for() 提供精确能力声明。

// ─── 3. ProviderMeta — 连接元数据（框架环境约定、控制层）──

/// 连接元数据 trait — 框架环境约定。
pub trait ProviderMeta: Send + Sync {
    /// Provider 标识（全小写，如 "openai", "anthropic"）。
    fn provider_id(&self) -> &str;

    /// 默认基础 URL（当 `<PREFIX>_BASE_URL` 未设置时使用）。
    fn default_base_url(&self) -> &'static str;

    /// 认证方式（决定使用 `bearer()` 还是 `header()` 构造配置）。
    fn auth_style(&self) -> AuthStyle;

    /// 该 Codec 声明的默认 Headers。
    ///
    /// 与 Builder 传入的 extra_headers 以及 CodecRequest 的 headers 三层合并：
    /// codec defaults → builder headers → request headers（后者覆盖前者）。
    ///
    /// 默认返回空 HeaderMap。子类可 override 以声明协议必需的 Header，
    /// 如 Anthropic 的 `anthropic-version` 或 OpenRouter 的 `HTTP-Referer`。
    fn default_headers(&self) -> HeaderMap {
        HeaderMap::new()
    }

    /// API Key 环境变量名，默认 `{PROVIDER_ID}_API_KEY`。
    ///
    /// 子类可 override 以支持非标准命名（如 `DASHSCOPE_API_KEY` vs `QWEN_API_KEY`），
    /// 或支持多实例（`OPENAI_API_KEY_PRIMARY`）。
    fn api_key_env(&self) -> Cow<'static, str> {
        format!("{}_API_KEY", self.provider_id().to_ascii_uppercase()).into()
    }
}

// ─── ProviderExtension — 生态扩展统一入口 ───

/// 生态扩展统一插件接口。
///
/// 开发者实现一个新 Provider 时，只需同时满足这三个轴向的职责。
/// 框架内部按需消费子 trait（如 `process_stream` 只需 `ChatCodec`）。
pub trait ProviderExtension: ChatCodec + ModelCapabilities + ProviderMeta {}

// 毯式实现：任何同时实现了这三个 trait 的类型，自动成为合格的 ProviderExtension。
impl<T> ProviderExtension for T where T: ChatCodec + ModelCapabilities + ProviderMeta {}

// NOTE: ProviderCodec 已拆分为 ChatCodec + ModelCapabilities + ProviderMeta。
// 框架内部使用 ProviderExtension 超级 trait。
