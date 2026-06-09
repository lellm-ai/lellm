//! Agent 运行时 — 编排循环、防御机制、工具系统。
//!
//! 提供 ToolUseLoop、AgentBuilder 以及防御机制
//! （循环检测、重试策略、Fallback 等）。

pub mod builder;
pub mod config;
pub mod context;
pub mod event;
pub mod fallback;
pub(crate) mod iteration;
#[cfg(feature = "v02-preview")]
pub mod loop_detector;
pub mod request_opts;
pub mod retry;
pub mod runtime;
#[cfg(feature = "v02-preview")]
pub mod signal_voter;
pub mod tools;

// ─── 工具系统 re-export ──────────────────────────────────────────

pub use tools::{
    ToolArgs, ToolCategory, ToolExecutor, ToolRegistration, ToolRegistry, ToolSearchResult,
    ToolSource,
};

// ─── 运行时 re-export ────────────────────────────────────────────

pub use builder::AgentBuilder;
pub use config::{ToolUseConfig, ToolUseDeps};
pub use context::{
    CompactionResult, ContextBudget, ContextCompactor, LocalCompactor, estimate_text,
    estimate_tokens,
};
pub use event::{AgentEvent, AgentStream, StopReason};
pub use fallback::{DefaultFallback, FallbackAction, FallbackContext, FallbackStrategy};
pub use lellm_provider::ResolvedModel;
#[cfg(feature = "v02-preview")]
pub use loop_detector::{LoopDetector, LoopIntervention};
pub use request_opts::RequestOptions;
pub use retry::{BackoffStrategy, RetryPolicy};
pub use runtime::{LoopState, ToolUseLoop, ToolUseResult};
#[cfg(feature = "v02-preview")]
pub use signal_voter::{NegativeSignal, SignalVoter};

// 从 core 再导出，方便用户统一从 lellm::agent 引入
pub use lellm_core::{ToolError, ToolErrorKind, ToolResult};

// 从 tools re-export
pub use tools::{BatchExecutionResult, ParallelSafety};
