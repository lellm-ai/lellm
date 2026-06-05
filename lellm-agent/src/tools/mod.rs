//! 工具系统 — Agent Runtime 的组成部分。
//!
//! 提供 ToolRegistry, ToolExecutor, ToolUseLoop 以及防御机制
//! （循环检测、重试策略、Fallback 等）。

pub mod executor;
pub mod fallback;
#[cfg(feature = "v02-preview")]
pub mod loop_detector;
pub mod registry;
pub mod retry;
pub mod runtime;
#[cfg(feature = "v02-preview")]
pub mod signal_voter;

pub use executor::{ParallelSafety, ToolCategory, ToolExecutor, ToolRegistration};
pub use fallback::{DefaultFallback, FallbackAction, FallbackContext, FallbackStrategy};
pub use lellm_provider::ResolvedModel;
#[cfg(feature = "v02-preview")]
pub use loop_detector::{LoopDetector, LoopIntervention};
pub use registry::{ToolRegistry, ToolSearchResult, ToolSource};
pub use retry::{BackoffStrategy, RetryPolicy};
pub use runtime::{LoopState, ToolUseLoop, ToolUseResult};
#[cfg(feature = "v02-preview")]
pub use signal_voter::{NegativeSignal, SignalVoter};

/// 异步工具函数类型（executor + retry 共享）
pub(crate) type ToolFn = std::sync::Arc<
    dyn Fn(
            &serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send>>
        + Send
        + Sync,
>;

// 从 core 再导出，方便用户统一从 lellm::agent 引入
pub use lellm_core::{ToolError, ToolErrorKind, ToolResult};

/// Agent 层流式事件 — 封闭、强类型、exhaustive match
///
/// 终态契约：
/// - 正常结束：`LoopEnd` 恰好一次，然后 channel 关闭
/// - 异常结束：`LoopError` 恰好一次，然后 channel 关闭
/// - 终态事件后不再发送任何事件
#[derive(Debug)]
pub enum AgentEvent {
    /// Provider 层事件
    Provider(lellm_provider::ProviderEvent),
    /// 工具开始执行
    ToolStart { tool_call_id: String, name: String },
    /// 工具执行完成
    ToolEnd {
        tool_call_id: String,
        result: ToolResult,
    },
    /// 工具重试
    Retry {
        tool_call_id: String,
        attempt: usize,
        max_attempts: usize,
        reason: String,
    },
    /// Agent loop 正常结束（恰好一次，后不再发送）
    LoopEnd { result: ToolUseResult },
    /// Agent loop 异常结束（恰好一次，后不再发送）— 不含 messages，消费者自行维护
    LoopError {
        error: lellm_core::LlmError,
        iterations: usize,
    },
}

/// Agent loop 停止原因 — 描述"为什么停止"，而非"响应长什么样"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Agent 已获得最终答案并正常结束
    Complete,
    /// 达到最大轮次
    MaxIterationsReached,
    /// 检测到循环（v0.2）
    LoopDetected,
}

/// Agent 层流式事件通道类型别名
pub type AgentStream = tokio::sync::mpsc::Receiver<AgentEvent>;
