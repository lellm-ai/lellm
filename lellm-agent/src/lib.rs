//! lellm-agent — Agent 运行时。
//!
//! 提供完整的 Agent 运行时能力：工具系统、记忆管理、Agent Loop、
//! 循环检测、重试策略、Fallback 降级等。

pub mod memory;
pub mod tools;

pub use memory::ShortTermMemory;
pub use tools::{
    AgentEvent, AgentStream, BackoffStrategy, DefaultFallback, FallbackAction, FallbackContext,
    FallbackStrategy, ParallelSafety, ResolvedModel, RetryPolicy, StopReason, ToolCategory,
    ToolError, ToolErrorKind, ToolExecutor, ToolRegistration, ToolRegistry, ToolResult,
    ToolSearchResult, ToolSource, ToolUseLoop, ToolUseResult,
};
#[cfg(feature = "v02-preview")]
pub use tools::{LoopDetector, LoopIntervention, NegativeSignal, SignalVoter};
