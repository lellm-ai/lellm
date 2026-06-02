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

/// Agent 层流式事件
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Provider 层事件
    Provider(lellm_provider::ProviderEvent),
    /// 工具开始执行
    ToolStart { tool_call_id: String, name: String },
    /// 工具执行完成
    ToolEnd {
        tool_call_id: String,
        result: String,
    },
    /// 重试
    Retry { reason: String },
    /// 自定义事件
    Custom { data: serde_json::Value },
}
