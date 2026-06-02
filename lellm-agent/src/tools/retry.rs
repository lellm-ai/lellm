//! 工具重试策略 — 错误分类、退避、提示注入。

use std::time::Duration;

use super::ToolCallResult;

/// 工具错误类型分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    Timeout,
    PermissionDenied,
    NotFound,
    NetworkError,
    ParseError,
    Unknown,
}

impl ToolErrorKind {
    pub fn is_retriable(self) -> bool {
        matches!(self, Self::Timeout | Self::NetworkError | Self::Unknown)
    }

    pub fn max_attempts(self) -> u32 {
        match self {
            Self::Timeout => 5,
            Self::NetworkError => 3,
            Self::Unknown => 3,
            _ => 0,
        }
    }

    pub fn backoff_ms(self, attempt: u32) -> u64 {
        match self {
            Self::Timeout => (2_u64).saturating_pow(attempt + 1) * 1000,
            Self::NetworkError | Self::Unknown => 3000,
            _ => 0,
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Timeout => "该操作超时，请检查参数或尝试更轻量的替代工具",
            Self::PermissionDenied => "权限不足，请确认当前角色是否允许此操作",
            Self::NotFound => "资源未找到，请检查参数拼写",
            Self::NetworkError => "网络异常，请重试或考虑降级方案",
            Self::ParseError => "输出格式不匹配，请严格遵循 JSON Schema",
            Self::Unknown => "操作失败，请分析错误信息并调整策略",
        }
    }
}

/// 退避策略
#[derive(Debug, Clone)]
pub enum BackoffStrategy {
    Fixed(Duration),
    Exponential { base: Duration, max: Duration },
}

/// 重试策略
pub struct RetryPolicy;

impl RetryPolicy {
    pub async fn execute_with_retry<F, Fut>(kind: ToolErrorKind, f: F) -> ToolCallResult
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = ToolCallResult>,
    {
        let max = kind.max_attempts();
        if max == 0 {
            return f().await;
        }

        for attempt in 0..max {
            let result = f().await;
            match result {
                ToolCallResult::Ok(_) => return result,
                ToolCallResult::Err(msg) if attempt == max - 1 => {
                    return ToolCallResult::Err(format!(
                        "{} (retried {} times, hint: {})",
                        msg,
                        max,
                        kind.hint()
                    ));
                }
                ToolCallResult::Err(_) => {
                    let delay = kind.backoff_ms(attempt);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                }
            }
        }
        f().await
    }
}
