//! lellm-agent — Agent 运行时。
//!
//! 提供完整的 Agent 运行时能力：工具系统、记忆管理、Agent Loop、
//! 循环检测、重试策略、Fallback 降级等。

pub mod memory;
pub mod tools;

// Re-export schemars so derive(ToolDefinition) macro can reference it.
pub use schemars;

pub use memory::ShortTermMemory;
pub use tools::{
    AgentBuilder, AgentEvent, AgentStream, BackoffStrategy, DefaultFallback, FallbackAction,
    FallbackContext, FallbackStrategy, ParallelSafety, ResolvedModel, RetryPolicy, StopReason,
    ToolArgs, ToolCategory, ToolError, ToolErrorKind, ToolExecutor, ToolRegistration,
    ToolRegistry, ToolResult, ToolSearchResult, ToolSource, ToolUseConfig, ToolUseDeps,
    ToolUseLoop, ToolUseResult,
};
#[cfg(feature = "v02-preview")]
pub use tools::{LoopDetector, LoopIntervention, NegativeSignal, SignalVoter};

// ─── 糖衣 API（第三层原型） ───

/// 便捷创建 Agent — 生态包糖衣 API 原型。
///
/// 这是未来 `lellm-openai` / `lellm-anthropic` 等生态包中
/// `create_agent()` 的简化版本。
///
/// # 示例
/// ```ignore
/// use lellm_agent::{create_agent, ToolRegistration};
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
    AgentBuilder::new(model).build()
}

/// 快速创建带工具的 Agent。
pub fn create_agent_with_tools(
    model: ResolvedModel,
    tools: impl IntoIterator<Item = ToolRegistration>,
) -> ToolUseLoop {
    AgentBuilder::new(model).tools(tools).build()
}

/// 快速创建带系统提示的 Agent。
pub fn create_agent_with_system(model: ResolvedModel, system_prompt: String) -> ToolUseLoop {
    AgentBuilder::new(model)
        .system_prompt(system_prompt)
        .build()
}

/// 完整配置的便捷创建。
pub fn create_agent_full(
    model: ResolvedModel,
    system_prompt: String,
    tools: impl IntoIterator<Item = ToolRegistration>,
    max_iterations: usize,
) -> ToolUseLoop {
    AgentBuilder::new(model)
        .system_prompt(system_prompt)
        .tools(tools)
        .max_iterations(max_iterations)
        .build()
}
