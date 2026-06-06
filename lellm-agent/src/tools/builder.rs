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
use super::executor::ToolExecutor;
use super::fallback::FallbackStrategy;
use super::retry::RetryPolicy;
use super::runtime::ToolUseLoop;

/// Agent 链式构建器 — 推荐的 Agent 创建方式。
///
/// 内部持有构建参数，`build()` 时组装为 `ToolUseLoop`。
/// 所有 setter 返回 `self`（不借用），支持流畅的链式调用。
pub struct AgentBuilder {
    model: ResolvedModel,
    executor: ToolExecutor,
    max_iterations: usize,
    fallback: Option<Arc<dyn FallbackStrategy>>,
    system_prompt: Option<String>,
}

impl AgentBuilder {
    /// 创建构建器，绑定模型。
    pub fn new(model: ResolvedModel) -> Self {
        Self {
            model,
            executor: ToolExecutor::default(),
            max_iterations: 10,
            fallback: None,
            system_prompt: None,
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
        self.max_iterations = max;
        self
    }

    /// 设置系统提示。
    pub fn system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = Some(prompt);
        self
    }

    /// 设置 Fallback 策略。
    pub fn fallback(mut self, fallback: Arc<dyn FallbackStrategy>) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// 设置工具重试策略。
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.executor = ToolExecutor::with_retry_policy(policy);
        self
    }

    /// 构建 ToolUseLoop。
    pub fn build(self) -> ToolUseLoop {
        let mut loop_ = ToolUseLoop::new(self.model, self.executor);

        loop_.max_iterations = self.max_iterations;

        if let Some(sp) = self.system_prompt {
            loop_.system_prompt = Some(sp);
        }

        if let Some(fb) = self.fallback {
            loop_.fallback = fb;
        }

        loop_
    }
}

impl AgentBuilder {
    /// 从现有 ToolUseLoop 创建构建器（用于修改配置）。
    pub fn from_loop(loop_: &ToolUseLoop) -> Self {
        Self {
            model: loop_.model.clone(),
            executor: loop_.executor.clone(),
            max_iterations: loop_.max_iterations,
            fallback: Some(loop_.fallback.clone()),
            system_prompt: loop_.system_prompt.clone(),
        }
    }
}
