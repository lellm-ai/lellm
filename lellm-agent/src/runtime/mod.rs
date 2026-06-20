//! Agent 运行时 — 编排循环、防御机制、工具系统。
//!
//! 提供 ToolUseLoop、AgentBuilder 以及防御机制
//! （重试策略、Fallback 等）。

pub mod builder;
pub mod config;
pub mod context;
pub mod event;
pub mod fallback;
pub mod flow_node;
pub(crate) mod iteration;
pub mod react;
pub mod request_opts;
pub mod retry;
#[allow(clippy::module_inception)]
pub mod runtime;
pub mod tools;

// ─── 工具系统 re-export ──────────────────────────────────────────

pub use tools::{
    BatchExecutionResult, CompositeCatalog, ParallelSafety, StaticCatalog, ToolArgs, ToolCatalog,
    ToolCategory, ToolExecutor, ToolRegistration, ToolSnapshot, execute_batch_with,
};

// ─── 运行时 re-export ────────────────────────────────────────────

pub use builder::AgentBuilder;
pub use config::{ToolUseConfig, ToolUseDeps};
pub use context::{
    CompactionResult, ContextBudget, ContextCompactor, LocalCompactor, estimate_message,
    estimate_text, estimate_tokens,
};
pub use event::{AgentEvent, AgentStream, StopReason};
pub use fallback::{DefaultFallback, FallbackAction, FallbackContext, FallbackStrategy};
pub use lellm_provider::ResolvedModel;
pub use request_opts::RequestOptions;
pub use retry::{BackoffStrategy, RetryPolicy};
pub use runtime::{ResolvedRound, ToolUseLoop, ToolUseResult};

// 从 core 再导出，方便用户统一从 lellm::agent 引入
pub use lellm_core::{IntoToolError, IntoToolResult, ToolError, ToolErrorKind, ToolResult};

// FlowNode 适配
pub use flow_node::AgentFlowNode;
