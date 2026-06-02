//! 降级策略 — 可注入的 Fallback 回调。

use async_trait::async_trait;
use lellm_core::{ChatRequest, Message};

/// 降级原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    SignalEscalation,
    LoopDetected,
    MaxIterationsExceeded,
    Timeout,
}

impl std::fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FallbackReason::SignalEscalation => write!(f, "signal escalation"),
            FallbackReason::LoopDetected => write!(f, "loop detected"),
            FallbackReason::MaxIterationsExceeded => write!(f, "max iterations exceeded"),
            FallbackReason::Timeout => write!(f, "timeout"),
        }
    }
}

/// 降级执行结果
#[derive(Debug)]
pub struct FallbackResult {
    pub success: bool,
    pub output: String,
    pub reason: Option<FallbackReason>,
    pub token_usage: u32,
}

impl FallbackResult {
    pub fn success(output: impl Into<String>, token_usage: u32) -> Self {
        Self {
            success: true,
            output: output.into(),
            reason: None,
            token_usage,
        }
    }

    pub fn failure(output: impl Into<String>, reason: FallbackReason) -> Self {
        Self {
            success: false,
            output: output.into(),
            reason: Some(reason),
            token_usage: 0,
        }
    }
}

/// Fallback 上下文 — 传递给回调的信息
pub struct FallbackContext {
    pub original_request: ChatRequest,
    pub iterations: usize,
    pub error: Option<String>,
    pub messages: Vec<Message>,
}

/// Fallback 策略 trait — 可注入的回调
#[async_trait]
pub trait FallbackStrategy: Send + Sync {
    fn reason(&self) -> FallbackReason;
    async fn handle(&self, context: &FallbackContext) -> FallbackResult;
}

/// 默认 fallback — 将错误信息注入回对话
pub struct DefaultFallback {
    reason: FallbackReason,
}

impl DefaultFallback {
    pub fn new(reason: FallbackReason) -> Self {
        Self { reason }
    }
}

#[async_trait]
impl FallbackStrategy for DefaultFallback {
    fn reason(&self) -> FallbackReason {
        self.reason
    }

    async fn handle(&self, ctx: &FallbackContext) -> FallbackResult {
        FallbackResult::failure(
            format!(
                "tool loop failed: {:?}, executed {} iterations",
                ctx.error, ctx.iterations
            ),
            self.reason,
        )
    }
}
