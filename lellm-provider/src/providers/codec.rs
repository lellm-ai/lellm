//! ProviderCodec — 协议编解码 SPI。
//!
//! 将 `ProviderAdapter` 的职责拆分为三类：
//! - **协议编解码**：`encode()` / `decode()` / `decode_sse()` — Codec 的核心职责
//! - **连接元数据**：`provider_id()` / `default_base_url()` / `auth_style()` — 框架约定
//! - **能力声明**：`capabilities_for()` — 模型感知的能力矩阵
//!
//! `CodecProvider`（原 `GenericProvider`）持有 Codec + 连接配置，
//! 统一负责 HTTP 发送、认证注入、超时控制。

use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{ChatRequest, ChatResponse, LlmError};
use std::borrow::Cow;

use super::stream::sse_frame::SseFrame;
pub(crate) use super::stream::tool_call_accumulator::ToolCallDelta;

// ─── Codec 编解码中间表示 ───

/// Codec 请求 — Codec 构建，CodecProvider 发送。
///
/// Codec 只关心协议适配（路径、Header、Body），
/// 不关心 base_url、认证、HTTP Client。
#[derive(Debug)]
pub(crate) struct CodecRequest {
    /// 相对路径。例如 `/v1/chat/completions` 或 `/v1beta/models/gemini-pro:generateContent`。
    pub path: Cow<'static, str>,
    /// 该厂商特有的自定义 Headers。例如 Anthropic 的 `anthropic-version: 2023-06-01`。
    pub headers: HeaderMap,
    /// 序列化后的请求体。
    pub body: Bytes,
}

/// 流式 chunk — Codec 解析 SseFrame 后返回。
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
    Usage(lellm_core::TokenUsage),
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

// ─── ProviderCodec SPI (pub(crate)) ───

/// Provider 协议编解码器 trait。
///
/// Codec **不知道** `CodecProvider`、`reqwest`、HTTP。
/// 只负责：`ChatRequest → CodecRequest`（编码），`body bytes → ChatResponse`（解码）。
///
/// 连接元数据（provider_id / base_url / auth）与能力声明也在此 trait 上，
/// 便于 `CodecProvider::from_env()` 自动推导配置。
pub(crate) trait ProviderCodec: Send + Sync {
    // ── 连接元数据（框架约定）──

    /// Provider 标识（全小写，如 "openai", "anthropic"）。
    /// 环境变量前缀自动推导为 `provider_id().to_ascii_uppercase()`。
    fn provider_id(&self) -> &str;

    /// 默认基础 URL（当 `<PREFIX>_BASE_URL` 未设置时使用）。
    fn default_base_url(&self) -> &'static str;

    /// 认证方式（决定使用 `bearer()` 还是 `header()` 构造配置）。
    fn auth_style(&self) -> AuthStyle;

    // ── 协议编解码（核心职责）──

    /// 编码 ChatRequest 为 CodecRequest（路径 + 协议 Header + JSON Body 字节）。
    fn encode(&self, req: &ChatRequest, stream: bool) -> Result<CodecRequest, LlmError>;

    /// 解码成功响应 body（2xx）为 ChatResponse。
    fn decode(&self, body: &[u8]) -> Result<ChatResponse, LlmError>;

    /// 解码单个 SSE 帧的 data 字段。
    /// SSE 协议解析（缓冲、行拆分、event/data 提取）由 CodecProvider 统一处理，构建 SseFrame。
    fn decode_sse(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError>;

    // ── 能力声明（模型感知）──

    /// 返回该 model 支持的能力矩阵。
    ///
    /// 若 model 未知，返回最保守的默认值（全 false）。
    /// Codec 可结合精确 match + 启发式模糊匹配来识别主流模型。
    fn capabilities_for(&self, _model: &str) -> Capabilities {
        Capabilities::default()
    }
}
