//! 工具系统 — Agent Runtime 的组成部分。
//!
//! 提供 ToolRegistry, ToolExecutor, ToolUseLoop 以及防御机制
//! （循环检测、重试策略、Fallback 等）。

pub mod executor;
pub mod fallback;
pub mod loop_;
pub mod loop_detector;
pub mod registry;
pub mod retry;
pub mod signal_voter;

pub use executor::{ParallelSafety, ToolCategory, ToolExecutor, ToolRegistration};
pub use fallback::{
    DefaultFallback, FallbackAction, FallbackContext, FallbackReason, FallbackStrategy,
};
pub use lellm_provider::ResolvedModel;
pub use loop_::{ToolCallResult, ToolUseLoop, ToolUseResult};
pub use loop_detector::{LoopDetector, LoopIntervention};
pub use registry::{ToolRegistry, ToolSearchResult, ToolSource};
pub use retry::{BackoffStrategy, RetryPolicy, ToolErrorKind};
pub use signal_voter::{NegativeSignal, SignalVoter};

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
        result: ToolCallResult,
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
    /// Agent loop 异常结束（恰好一次，后不再发送）
    LoopError {
        error: lellm_core::LlmError,
        iterations: usize,
        messages: Vec<lellm_core::Message>,
    },
}

/// Agent loop 停止原因 — 描述"为什么停止"，而非"响应长什么样"
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Agent 已获得最终答案并正常结束
    Complete,
    /// 达到最大轮次
    MaxIterationsReached,
    /// 检测到循环
    LoopDetected,
    /// Fallback 主动给出最终结果
    FallbackComplete,
}

/// Agent 层流式事件通道类型别名
pub type AgentStream = tokio::sync::mpsc::Receiver<AgentEvent>;
