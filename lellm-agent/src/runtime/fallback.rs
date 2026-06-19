//! 降级策略 — Provider 错误时的恢复决策。
//!
//! v0.1 范围：仅处理 `LlmError`（Provider 调用失败），支持 `Retry` / `Abort`。
//! v0.2 扩展：`ToolError`、`LoopDetected`、`MaxIterationsReached` 等触发条件。

use std::sync::Arc;

use async_trait::async_trait;
use lellm_core::{LlmError, Message};

/// Fallback 上下文 — 提供给策略做决策的依据。
///
/// **设计原则：**
/// - Runtime 只报告事实（第几次失败、什么错误），策略自己决定是否继续
/// - `error` 为借用 — Context 是**观察窗口**，不成为错误的临时仓库
/// - 错误所有权始终留在 Retry Loop 手中，Abort 时直接 `err.clone()` 返回
pub struct FallbackContext<'a> {
    /// Provider 调用失败的具体错误（借用，Context 只观察）
    pub error: &'a LlmError,
    /// 当前失败次数（从 1 开始，首次失败为 1）
    pub attempt: usize,
    /// Agent Loop 已完成的迭代轮次
    pub iterations: usize,
    /// 当前对话历史（不可变快照）
    pub conversation: Arc<[Message]>,
}

/// Fallback 动作 — 策略返回的决策结果。
///
/// v0.1 仅使用 `Retry` / `Abort`。
#[derive(Debug, Clone)]
pub enum FallbackAction {
    /// 重试同一请求（Runtime 重新调用 provider）
    Retry,
    /// 终止并返回错误
    Abort,
    // TODO(v0.2): 注入干预消息后重试
    // RetryWithMessages(Vec<Message>),
    // TODO(v0.2): 切换到备选 provider
    // SwitchProvider(String),
    // TODO(v0.2): 策略直接给出最终响应
    // Complete(ChatResponse),
}

/// Fallback 策略 trait — 可注入的恢复决策点。
#[async_trait]
pub trait FallbackStrategy: Send + Sync {
    /// 处理降级信号，返回决策动作。
    async fn handle(&self, ctx: &FallbackContext) -> FallbackAction;
}

/// 默认 fallback 策略 — 可重试错误自动重试，其余直接终止。
pub struct DefaultFallback {
    max_retries: usize,
}

impl DefaultFallback {
    pub fn new(max_retries: usize) -> Self {
        Self { max_retries }
    }

    /// 判断 LlmError 是否可重试
    fn is_retriable(error: &LlmError) -> bool {
        match error {
            LlmError::Timeout { .. } | LlmError::Network { .. } => true,
            LlmError::Provider {
                status: Some(s), ..
            } => *s >= 500,
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
        if Self::is_retriable(ctx.error) && ctx.attempt < self.max_retries {
            FallbackAction::Retry
        } else {
            FallbackAction::Abort
        }
    }
}
