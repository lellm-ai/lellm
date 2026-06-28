//! LlmInvoker — 封装 LLM 调用的防御策略层。
//!
//! **职责单一：** 只负责"获得一次成功的 LLM 调用"。
//! 不感知 Agent 循环、工具执行、预算管理等概念。
//!
//! # 分层
//!
//! ```text
//! LLMNode  — 只负责 State ↔ Request ↔ Effects
//!    │
//! LlmInvoker — 只负责防御策略（retry, fallback, stream state machine）
//!    │
//! LlmProvider — 只负责 protocol adapter（stateless）
//! ```
//!
//! # Stream State Machine
//!
//! 流式调用的状态决定了是否可以安全重试：
//!
//! ```text
//! NotStarted ──stream opened──> HeadersReceived ──first event──> FirstChunkSent ──EOF──> Finished
//!    │                              │                        │
//!    │ retry OK                     │ retry OK               │ ❌ abort (tokens sent)
//!    └── provider.stream() 失败     └── 尚未消费事件
//! ```
//!
//! 一旦 FirstChunkSent（token 已发送给消费者），禁止重试 —
//! 因为 token 是不可撤销的，重试会导致重复输出。

use std::sync::Arc;

use futures_core::Stream;
use lellm_core::{ChatRequest, LlmError, Message};
use lellm_provider::{ProviderEvent, ProviderStream, ResolvedModel};
use std::pin::Pin;
use std::task::{Context, Poll};

use super::config::ToolUseConfig;
use super::fallback::{FallbackAction, FallbackContext, FallbackStrategy};
use super::retry::BackoffStrategy;

// ─── Stream State Machine ───────────────────────────────────────

/// 流式调用状态 — 决定 retry 边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// 尚未打开流 — retry OK
    NotStarted,
    /// 流已打开但尚未消费任何事件 — retry OK
    HeadersReceived,
    /// 至少一个事件已发送给消费者 — ❌ abort (tokens 不可撤销)
    FirstChunkSent,
    /// 流已完成 — impossible (不应再尝试)
    Finished,
}

impl StreamState {
    /// 当前状态是否允许重试。
    pub fn can_retry(&self) -> bool {
        matches!(self, Self::NotStarted | Self::HeadersReceived)
    }
}

// ─── LlmInvoker ─────────────────────────────────────────────────

/// LLM 调用器 — 封装防御策略。
///
/// 内部持有 `ResolvedModel` + `FallbackStrategy` + 重试配置。
/// 提供 `invoke_stream()` 方法，自动处理流打开前的重试。
#[derive(Clone)]
pub struct LlmInvoker {
    /// 解析后的模型（provider + model name）
    model: ResolvedModel,
    /// 降级策略
    fallback: Arc<dyn FallbackStrategy>,
    /// 最大重试次数（流打开失败时）
    max_retries: usize,
    /// 重试退避策略
    backoff: BackoffStrategy,
}

impl LlmInvoker {
    /// 创建 LlmInvoker。
    pub fn new(
        model: ResolvedModel,
        fallback: Arc<dyn FallbackStrategy>,
        max_retries: usize,
        backoff: BackoffStrategy,
    ) -> Self {
        Self {
            model,
            fallback,
            max_retries,
            backoff,
        }
    }

    /// 从配置构建 LlmInvoker。
    pub fn from_config(
        model: ResolvedModel,
        config: &ToolUseConfig,
        fallback: Arc<dyn FallbackStrategy>,
    ) -> Self {
        Self::new(
            model,
            fallback,
            config.retry_policy.max_attempts() as usize,
            config.retry_policy.backoff().clone(),
        )
    }

    /// 获取模型引用。
    pub fn model(&self) -> &ResolvedModel {
        &self.model
    }

    /// 执行流式调用，自动处理流打开前的重试。
    ///
    /// # 重试逻辑
    ///
    /// 1. 调用 `provider.stream(req)`
    /// 2. 如果失败且尚未发送任何 token：
    ///    a. 查询 fallback 策略（Retry / Abort）
    ///    b. Retry → 退避后重试
    ///    c. Abort → 返回错误
    /// 3. 如果成功打开流 → 返回 `RetryAwareStream`
    ///
    /// # 重要
    ///
    /// 一旦流开始发送数据（FirstChunkSent），禁止重试。
    /// 流消费期间的错误由调用方处理。
    pub async fn invoke_stream(
        &self,
        req: &ChatRequest,
        messages: &[Message],
        iteration: usize,
    ) -> Result<RetryAwareStream, LlmError> {
        let mut attempt = 1;
        let mut stream_state = StreamState::NotStarted;

        loop {
            match self.model.provider.stream(req).await {
                Ok(stream) => {
                    return Ok(RetryAwareStream::new(stream, attempt));
                }
                Err(ref err) => {
                    tracing::warn!(
                        attempt = attempt,
                        error = %err,
                        stream_state = ?stream_state,
                        "LLM stream invocation failed"
                    );

                    // 如果已经发送了数据，禁止重试
                    if !stream_state.can_retry() {
                        return Err(err.clone());
                    }

                    // 查询 fallback 策略
                    let ctx = FallbackContext {
                        error: err,
                        attempt,
                        iterations: iteration,
                        conversation: Arc::from(messages),
                    };

                    match self.fallback.handle(&ctx).await {
                        FallbackAction::Retry => {
                            if attempt >= self.max_retries {
                                tracing::warn!(
                                    attempt,
                                    max_retries = self.max_retries,
                                    "max retries reached, aborting"
                                );
                                return Err(err.clone());
                            }

                            let delay = self.backoff.delay(attempt as u32);
                            tracing::info!(
                                attempt = attempt + 1,
                                max_retries = self.max_retries,
                                delay_ms = delay.as_millis(),
                                "retrying LLM invocation"
                            );
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                            stream_state = StreamState::HeadersReceived;
                        }
                        FallbackAction::Abort => {
                            return Err(err.clone());
                        }
                    }
                }
            }
        }
    }

    /// 执行非流式调用（带重试）。
    pub async fn invoke(
        &self,
        req: &ChatRequest,
        messages: &[Message],
        iteration: usize,
    ) -> Result<lellm_core::ChatResponse, LlmError> {
        let mut attempt = 1;

        loop {
            match self.model.provider.call(req).await {
                Ok(response) => return Ok(response),
                Err(ref err) => {
                    tracing::warn!(
                        attempt = attempt,
                        error = %err,
                        "LLM invocation failed"
                    );

                    let ctx = FallbackContext {
                        error: err,
                        attempt,
                        iterations: iteration,
                        conversation: Arc::from(messages),
                    };

                    match self.fallback.handle(&ctx).await {
                        FallbackAction::Retry => {
                            if attempt >= self.max_retries {
                                return Err(err.clone());
                            }
                            let delay = self.backoff.delay(attempt as u32);
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                        }
                        FallbackAction::Abort => return Err(err.clone()),
                    }
                }
            }
        }
    }
}

// ─── RetryAwareStream ───────────────────────────────────────────

/// 包装 ProviderStream，追踪是否已发送数据。
///
/// 用于 LlmInvoker 判断是否可以安全重试。
pub struct RetryAwareStream {
    inner: Pin<Box<dyn Stream<Item = Result<ProviderEvent, LlmError>> + Send>>,
    /// 是否已发送至少一个数据事件
    has_sent_data: bool,
    /// 当前尝试编号
    attempt: usize,
}

impl RetryAwareStream {
    fn new(stream: ProviderStream, attempt: usize) -> Self {
        Self {
            inner: stream,
            has_sent_data: false,
            attempt,
        }
    }

    /// 检查是否已发送数据（决定是否可重试）。
    pub fn stream_state(&self) -> StreamState {
        if self.has_sent_data {
            StreamState::FirstChunkSent
        } else {
            StreamState::HeadersReceived
        }
    }

    /// 获取尝试编号。
    pub fn attempt(&self) -> usize {
        self.attempt
    }
}

impl Stream for RetryAwareStream {
    type Item = Result<ProviderEvent, LlmError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = Pin::new(&mut self.inner).poll_next(cx);
        if let Poll::Ready(Some(Ok(ref event))) = result {
            // ProviderEvent::Token 或 ThinkingDelta 表示数据已发送
            if matches!(
                event,
                ProviderEvent::Token { .. } | ProviderEvent::ThinkingDelta { .. }
            ) {
                self.has_sent_data = true;
            }
        }
        result
    }
}

// ─── 测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_state_can_retry() {
        assert!(StreamState::NotStarted.can_retry());
        assert!(StreamState::HeadersReceived.can_retry());
        assert!(!StreamState::FirstChunkSent.can_retry());
        assert!(!StreamState::Finished.can_retry());
    }
}
