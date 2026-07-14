//! Agent 运行时 — 编排循环、防御机制、工具系统。
//!
//! 提供 ToolUseLoop、AgentBuilder 以及防御机制
//! （重试策略、Fallback 等）。

pub mod builder;
pub mod config;
pub mod context;
pub mod event;
pub mod event_bridge;
pub mod fallback;
pub mod invoker;
pub mod react;
pub mod request_opts;
pub mod retry;
#[allow(clippy::module_inception)]
pub mod runtime;
pub mod stream_translation;
pub mod tools;
pub mod typed_state;

// ─── 工具系统 re-export ──────────────────────────────────────────

pub use tools::{
    BatchExecutionResult, CatalogDiagnostic, CompositeCatalog, ConflictPolicy, ExecutableTool,
    ParallelSafety, StaticCatalog, ToolArgs, ToolCatalog, ToolCategory, ToolExecutor, ToolFn,
    ToolSnapshot,
};

// ─── 运行时 re-export ────────────────────────────────────────────

pub use builder::AgentBuilder;
pub use config::{ToolCachePolicy, ToolUseConfig, ToolUseDeps};
pub use context::{
    CompactionResult, ContextBudget, ContextCompactor, LocalCompactor, estimate_message,
    estimate_text, estimate_tokens,
};
pub use event::{AgentEvent, AgentStream, StopReason};
pub use fallback::{DefaultFallback, FallbackAction, FallbackContext, FallbackStrategy};
pub use lellm_provider::ResolvedModel;
pub use react::StopConfig;
pub use request_opts::RequestOptions;
pub use retry::{BackoffStrategy, RetryPolicy};
pub use runtime::{ResolvedRound, ToolUseLoop, ToolUseResult};
pub use typed_state::{AgentMutation, AgentState};

// 从 core 再导出，方便用户统一从 lellm::agent 引入
pub use lellm_core::{IntoToolError, IntoToolResult, ToolError, ToolErrorKind, ToolResult};

// 流式事件翻译
pub use stream_translation::{AgentStreamEvent, TranslationResult, translate_provider_event};

// ─── MCP 集成 re-export (mcp feature) ────────────────────────────

#[cfg(feature = "mcp")]
pub use tools::mcp::{
    McpCatalog, McpCatalogWatcher, McpServerRegistry, NameConflictError, NameConflictPolicy,
    RegistryError, ServerConfig,
};

#[cfg(feature = "mcp")]
pub use tools::mcp::CatalogRefresh;
