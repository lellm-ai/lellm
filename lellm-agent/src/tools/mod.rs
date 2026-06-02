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

pub use executor::{ParallelSafety, ToolExecutor, ToolRegistration};
pub use fallback::{
    DefaultFallback, FallbackContext, FallbackReason, FallbackResult, FallbackStrategy,
};
pub use loop_::{ToolCallResult, ToolUseLoop, ToolUseResult};
pub use loop_detector::{LoopDetector, LoopIntervention};
pub use registry::{ToolRegistry, ToolSearchResult, ToolSource};
pub use retry::{BackoffStrategy, RetryPolicy, ToolErrorKind};
pub use signal_voter::{NegativeSignal, SignalVoter};
