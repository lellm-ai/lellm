//! 降级策略 — 可注入的 Fallback 回调。

use std::sync::Arc;

use async_trait::async_trait;
use lellm_core::{ChatResponse, LlmError, Message, ToolError};

/// 降级原因
#[derive(Debug)]
pub enum FallbackReason {
    LlmError(LlmError),
    ToolError(ToolError),
    LoopDetected,
    MaxIterationsReached,
}

/// Fallback 上下文
pub struct FallbackContext {
    pub reason: FallbackReason,
    pub conversation: Arc<[Message]>,
    pub attempt: usize,
    pub max_attempts: usize,
}

/// Fallback 动作
#[derive(Debug, Clone)]
pub enum FallbackAction {
    Retry,
    RetryWithMessages(Vec<Message>),
    SwitchProvider(String),
    Complete(ChatResponse),
    Abort,
}

/// Fallback 策略 trait
#[async_trait]
pub trait FallbackStrategy: Send + Sync {
    async fn handle(&self, ctx: &FallbackContext) -> FallbackAction;
}

/// 默认 fallback 策略
pub struct DefaultFallback {
    max_retries: usize,
}

impl DefaultFallback {
    pub fn new(max_retries: usize) -> Self {
        Self { max_retries }
    }

    /// 判断错误是否可重试
    fn is_retriable(error: &LlmError) -> bool {
        match error {
            LlmError::Timeout | LlmError::Network { .. } => true,
            LlmError::ApiError { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

impl Default for DefaultFallback {
    fn default() -> Self {
        Self::new(3)
    }
}

#[async_trait]
impl FallbackStrategy for DefaultFallback {
    async fn handle(&self, ctx: &FallbackContext) -> FallbackAction {
        match &ctx.reason {
            FallbackReason::LlmError(error) => {
                if Self::is_retriable(error) && ctx.attempt < self.max_retries {
                    FallbackAction::Retry
                } else {
                    FallbackAction::Abort
                }
            }
            FallbackReason::ToolError(_)
            | FallbackReason::LoopDetected
            | FallbackReason::MaxIterationsReached => FallbackAction::Abort,
        }
    }
}
