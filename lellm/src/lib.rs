//! LeLLM — Rust LLM orchestration framework.
//!
//! 默认开启 `provider`（core + provider 适配层）。
//!
//! ```toml
//! # 默认：core + provider
//! lellm = "0.4"
//!
//! # 需要 Graph 编排层
//! lellm = { version = "0.4", features = ["graph"] }
//!
//! # 需要 Agent 运行时
//! lellm = { version = "0.4", features = ["agent"] }
//!
//! # 需要 MCP 协议
//! lellm = { version = "0.4", features = ["mcp"] }
//!
//! # 全部启用
//! lellm = { version = "0.4", features = ["full"] }
//! ```

pub mod prelude {
    #[cfg(feature = "core")]
    pub use lellm_core::{
        ChatRequest, ChatResponse, ContentBlock, LlmError, Message, Prompt, ReasoningConfig,
        TokenUsage, ToolChoice, ToolDefinition, ToolError, ToolErrorKind, ToolResult,
    };

    #[cfg(feature = "tool")]
    pub use lellm_tool::{ToolArgs, compute_and_clean_schema, safe_fn};

    #[cfg(feature = "provider")]
    pub use lellm_provider::{
        Capabilities, CodecProvider, LlmProvider, ModelRouter, ProviderBuilder, ProviderEvent,
        ProviderRegistry, ProviderStream, ResolvedModel, RouteEntry, TaskLevel,
    };

    #[cfg(feature = "graph")]
    pub use lellm_graph::{
        BarrierDefaultAction, BarrierNode, BarrierSink, ChannelBarrierSink, ConditionNode, Graph,
        GraphBuilder, LeafContext, LeafNode, NoopBarrierSink, NoopStepCallback, SubgraphSpec,
        TaskNode, TerminalError,
    };

    #[cfg(feature = "agent")]
    pub use lellm_agent::{
        AgentBuilder, AgentEvent, AgentState, AgentStream, BackoffStrategy, CatalogDiagnostic,
        CompositeCatalog, ConflictPolicy, ContextBudget, ExecutableTool, FallbackAction,
        FallbackContext, FallbackStrategy, IntoToolError, IntoToolResult, LocalCompactor,
        ParallelSafety, RetryPolicy, StaticCatalog, StopReason, ToolArgs, ToolCatalog,
        ToolCategory, ToolExecutor, ToolFn, ToolSnapshot, ToolUseConfig, ToolUseDeps, ToolUseLoop,
        ToolUseResult, create_agent, create_agent_full, create_agent_with_system,
        create_agent_with_tools,
    };

    #[cfg(feature = "mcp")]
    pub use lellm_agent::{
        CatalogRefresh, McpCatalog, McpServerRegistry, NameConflictError, NameConflictPolicy,
        RegistryError, ServerConfig,
    };

    #[cfg(feature = "derive")]
    pub use lellm_derive::ToolDefinition;

    #[cfg(feature = "tool")]
    pub use lellm_tool::tool;
}

// ─── Crate 级 re-export（向后兼容）─────────────────────────────────

#[cfg(feature = "core")]
pub use lellm_core as core;

#[cfg(feature = "tool")]
pub use lellm_tool as tool;

#[cfg(feature = "provider")]
pub use lellm_provider as provider;

#[cfg(feature = "graph")]
pub use lellm_graph as graph;

#[cfg(feature = "agent")]
pub use lellm_agent as agent;

#[cfg(feature = "mcp")]
pub use lellm_mcp as mcp;

#[cfg(feature = "derive")]
pub use lellm_derive as derive;
