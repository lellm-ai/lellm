//! AgentBuilder — Agent 链式构建器。
//!
//! 提供推荐的入口 API，一步构建 ToolUseLoop。
//!
//! # 示例
//! ```ignore
//! use lellm_agent::{AgentBuilder, ToolRegistration};
//!
//! let agent = AgentBuilder::new(model)
//!     .system_prompt("你是一个有帮助的助手。".to_string())
//!     .tool(search_tool)
//!     .tool(weather_tool)
//!     .max_iterations(20)
//!     .build();
//! ```

use std::sync::Arc;

use lellm_provider::ResolvedModel;

use super::ToolRegistration;
use super::context::ContextBudget;
use super::executor::ToolExecutor;
use super::fallback::FallbackStrategy;
use super::retry::RetryPolicy;
use super::runtime::{ToolUseConfig, ToolUseDeps, ToolUseLoop};

/// Agent 链式构建器 — 推荐的 Agent 创建方式。
///
/// 内部持有构建参数，`build()` 时组装为 `ToolUseConfig` + `ToolUseDeps`，
/// 再传给 `ToolUseLoop::new()`。所有 setter 返回 `self`（不借用），
/// 支持流畅的链式调用。
pub struct AgentBuilder {
    model: ResolvedModel,
    executor: ToolExecutor,
    config: ToolUseConfig,
    deps: ToolUseDeps,
}

impl AgentBuilder {
    /// 创建构建器，绑定模型。
    pub fn new(model: ResolvedModel) -> Self {
        Self {
            model,
            executor: ToolExecutor::default(),
            config: ToolUseConfig::default(),
            deps: ToolUseDeps::default(),
        }
    }

    /// 注册工具。
    pub fn tool(mut self, reg: ToolRegistration) -> Self {
        let name = reg.definition.name.clone();
        self.executor.register(&name, reg);
        self
    }

    /// 批量注册工具。
    pub fn tools(mut self, registrations: impl IntoIterator<Item = ToolRegistration>) -> Self {
        for reg in registrations {
            let name = reg.definition.name.clone();
            self.executor.register(&name, reg);
        }
        self
    }

    /// 设置最大迭代轮次（默认 10）。
    pub fn max_iterations(mut self, max: usize) -> Self {
        self.config.max_iterations = max;
        self
    }

    /// 设置系统提示。
    pub fn system_prompt(mut self, prompt: String) -> Self {
        self.config.system_prompt = Some(prompt);
        self
    }

    /// 设置 Fallback 策略。
    pub fn fallback(mut self, fallback: Arc<dyn FallbackStrategy>) -> Self {
        self.deps.fallback = fallback;
        self
    }

    /// 设置工具重试策略（不影响已注册的工具）。
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.executor.set_retry_policy(policy);
        self
    }

    /// 设置上下文预算（Token 上限 + 压缩策略）。
    /// 若要关闭限制，设置 `max_tokens = usize::MAX`。
    pub fn context_budget(mut self, budget: ContextBudget) -> Self {
        self.config.context_budget = budget;
        self
    }

    /// 构建 ToolUseLoop。
    ///
    /// 将内部参数组装为 `ToolUseConfig` + `ToolUseDeps`，
    /// 传给 `ToolUseLoop::new()`。无字段复制逻辑。
    pub fn build(self) -> ToolUseLoop {
        ToolUseLoop::new(self.model, self.executor, self.config, self.deps)
    }
}
