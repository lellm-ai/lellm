//! 工具重试策略 — 瞬时故障恢复（"再试一次"）。
//!
//! 位于 ToolExecutor 内部，负责 transient failure recovery。
//! 重试耗尽后，错误向上传播至 FallbackStrategy（"换条路走"）。

use std::time::Duration;

use lellm_core::ToolResult;

use super::tools::ToolFn;

/// 退避策略
#[derive(Debug, Clone)]
pub enum BackoffStrategy {
    /// 固定间隔
    Fixed(Duration),
    /// 指数退避
    Exponential { base: Duration, max: Duration },
}

impl BackoffStrategy {
    /// 计算第 attempt 次的退避时间
    pub fn delay(&self, attempt: u32) -> Duration {
        match self {
            BackoffStrategy::Fixed(d) => *d,
            BackoffStrategy::Exponential { base, max } => {
                let d = base.saturating_mul(2_u32.pow(attempt));
                d.min(*max)
            }
        }
    }
}

/// 重试策略配置。
///
/// `max_attempts` 表示**总尝试次数**（初始执行 + 重试），与主流 SDK 语义一致：
/// - `max_attempts = 1` → 不重试，只执行一次
/// - `max_attempts = 3` → 初始执行 + 最多 2 次重试
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// 总尝试次数（初始 + 重试），默认 3
    max_attempts: u32,
    /// 退避策略
    backoff: BackoffStrategy,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff: BackoffStrategy::Exponential {
                base: Duration::from_millis(500),
                max: Duration::from_secs(30),
            },
        }
    }
}

impl RetryPolicy {
    pub fn new(max_attempts: u32, backoff: BackoffStrategy) -> Self {
        Self {
            max_attempts,
            backoff,
        }
    }

    /// 执行工具函数并自动重试可重试的错误。
    ///
    /// `max_attempts` = 总尝试次数（初始执行 + 重试），与 AWS SDK / reqwest 等语义一致。
    /// 执行链：`ToolUseLoop → ToolExecutor → RetryPolicy → tool_fn()`
    pub async fn execute_with_retry(
        &self,
        tool_fn: &ToolFn,
        args: &serde_json::Value,
    ) -> ToolResult {
        let mut last_result = tool_fn(args).await;
        if last_result.is_ok() {
            return last_result;
        }

        for attempt in 1..self.max_attempts {
            match &last_result {
                Err(e) if e.kind.is_retriable() => {}
                _ => return last_result,
            }

            let delay = self.backoff.delay(attempt);
            tracing::warn!(
                attempt,
                max = self.max_attempts,
                delay_ms = delay.as_millis(),
                "tool execution failed, retrying"
            );
            tokio::time::sleep(delay).await;

            last_result = tool_fn(args).await;
            if last_result.is_ok() {
                return last_result;
            }
        }

        last_result
    }
}
