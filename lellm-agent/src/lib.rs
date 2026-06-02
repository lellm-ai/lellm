//! lellm-agent — Agent 运行时。
//!
//! 提供完整的 Agent 运行时能力：工具系统、记忆管理、Agent Loop、
//! 循环检测、重试策略、Fallback 降级等。

pub mod memory;
pub mod tools;

pub use memory::ShortTermMemory;
pub use tools::{
    BackoffStrategy, DefaultFallback, FallbackAction, FallbackContext, FallbackReason,
    FallbackStrategy, LoopDetector, LoopIntervention, NegativeSignal, ParallelSafety,
    ResolvedModel, RetryPolicy, SignalVoter, ToolCallResult, ToolCategory, ToolErrorKind,
    ToolExecutor, ToolRegistration, ToolRegistry, ToolSearchResult, ToolSource, ToolUseLoop,
    ToolUseResult,
};
