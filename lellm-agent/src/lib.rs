//! lellm-agent — Agent 运行时。
//!
//! 提供完整的 Agent 运行时能力：工具系统、Agent Loop、
//! 循环检测、重试策略、Fallback 降级等。

pub mod hook;
pub mod runtime;

// Re-export schemars & serde so derive(Tool) / #[tool] macros can reference them.
pub use schemars;
pub use serde;

pub use hook::{AgentHook, AgentHookContext, AgentHookSnapshot, NoOpAgentHook, TracingAgentHook};
pub use runtime::{
    AgentBuilder, AgentEvent, AgentState, AgentStream, BackoffStrategy, BatchExecutionResult,
    CompactionResult, CompositeCatalog, ContextBudget, ContextCompactor, DefaultFallback,
    ExecutableTool, FallbackAction, FallbackContext, FallbackStrategy, IntoToolError,
    IntoToolResult, LocalCompactor, ParallelSafety, ResolvedModel, ResolvedRound, RetryPolicy,
    StaticCatalog, StopReason, ToolArgs, ToolCachePolicy, ToolCatalog, ToolCategory, ToolError,
    ToolErrorKind, ToolExecutor, ToolFn, ToolResult, ToolSnapshot, ToolUseConfig, ToolUseDeps,
    ToolUseLoop, ToolUseResult, estimate_message, estimate_tokens, execute_batch_with,
};

// ─── MCP 集成 re-export (mcp feature) ────────────────────────────

#[cfg(feature = "mcp")]
pub use runtime::{CatalogRefresh, McpCatalog, McpCatalogWatcher, McpServerRegistry, ServerConfig};

// 从 core 再导出 Prompt
pub use lellm_core::Prompt;

// ─── 糖衣 API（第三层原型） ───

/// 便捷创建 Agent — 生态包糖衣 API 原型。
///
/// 这是未来 `lellm-openai` / `lellm-anthropic` 等生态包中
/// `create_agent()` 的简化版本。
///
/// # 示例
/// ```ignore
/// use lellm_agent::{create_agent, ExecutableTool};
///
/// // 无工具的简单 Agent
/// let agent = create_agent(model);
///
/// // 带工具的 Agent
/// let agent = create_agent_with_tools(model, vec![search, weather]);
/// ```
///
/// 快速创建一个无工具的 Agent。
pub fn create_agent(model: ResolvedModel) -> ToolUseLoop {
    AgentBuilder::new(model).compile()
}

/// 快速创建带工具的 Agent。
pub fn create_agent_with_tools(
    model: ResolvedModel,
    tools: impl IntoIterator<Item = ExecutableTool>,
) -> ToolUseLoop {
    AgentBuilder::new(model).tools(tools).compile()
}

/// 快速创建带系统提示的 Agent。
pub fn create_agent_with_system(model: ResolvedModel, system: impl Into<Prompt>) -> ToolUseLoop {
    AgentBuilder::new(model).system(system).compile()
}

/// 完整配置的便捷创建。
pub fn create_agent_full(
    model: ResolvedModel,
    system: impl Into<Prompt>,
    tools: impl IntoIterator<Item = ExecutableTool>,
    max_iterations: usize,
) -> ToolUseLoop {
    AgentBuilder::new(model)
        .system(system)
        .tools(tools)
        .max_iterations(max_iterations)
        .compile()
}
