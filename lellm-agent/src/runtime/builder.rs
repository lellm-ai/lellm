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

use lellm_core::{ReasoningConfig, ToolChoice};
use lellm_provider::ResolvedModel;

use super::config::{ToolUseConfig, ToolUseDeps};
use super::context::ContextBudget;
use super::fallback::FallbackStrategy;
use super::request_opts::RequestOptions;
use super::retry::RetryPolicy;
use super::runtime::ToolUseLoop;
use super::tools::{ToolExecutor, ToolRegistration};

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

    /// 设置每次 LLM 请求的最大输出 token 数（默认 4k）。
    pub fn max_output_tokens(mut self, max: u32) -> Self {
        self.config.max_output_tokens = max;
        self
    }

    /// 设置整个 Agent Run 的最大输出 token 总数。
    ///
    /// 防止多轮工具调用导致总输出失控。达到阈值时立即停止，
    /// 返回 `StopReason::OutputBudgetExceeded`。
    pub fn max_total_output_tokens(mut self, max: u32) -> Self {
        self.config.max_total_output_tokens = Some(max);
        self
    }

    /// 设置系统提示。
    pub fn system_prompt(mut self, prompt: String) -> Self {
        self.config.system_prompt = Some(prompt);
        self
    }

    // ─── RequestOptions 快捷 setter ──────────────────────────

    /// 设置完整的 RequestOptions（覆盖所有生成参数）。
    ///
    /// 内部包裹 `ChatRequest`，与 core 层零重复定义。
    pub fn request_options(mut self, opts: RequestOptions) -> Self {
        self.config.request_options = opts;
        self
    }

    /// 设置生成温度（0.0 ~ 2.0）。
    pub fn temperature(mut self, t: f64) -> Self {
        self.config.request_options.chat_request.temperature = Some(t);
        self
    }

    /// 设置 nucleus sampling 阈值（0.0 ~ 1.0）。
    pub fn top_p(mut self, p: f64) -> Self {
        self.config.request_options.chat_request.top_p = Some(p);
        self
    }

    /// 设置随机种子，保证可复现性。
    pub fn seed(mut self, s: u64) -> Self {
        self.config.request_options.chat_request.seed = Some(s);
        self
    }

    /// 设置工具选择策略（仅首轮生效）。
    ///
    /// 第一轮强制指定工具选择，后续轮次由 LLM 自主决定。
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.config.request_options.chat_request.tool_choice = Some(choice);
        self
    }

    /// 设置停止序列。
    pub fn stop_sequences(mut self, seqs: Vec<String>) -> Self {
        self.config.request_options.chat_request.stop_sequences = Some(seqs);
        self
    }

    /// 设置预填充文本（引导模型输出方向）。
    pub fn prefill(mut self, text: String) -> Self {
        self.config.request_options.chat_request.prefill = Some(text);
        self
    }

    /// 设置推理配置（控制模型是否进行深度推理）。
    pub fn reasoning(mut self, r: ReasoningConfig) -> Self {
        self.config.request_options.chat_request.reasoning = Some(r);
        self
    }

    /// 设置是否流式输出推理过程。
    pub fn stream_thinking(mut self, enable: bool) -> Self {
        self.config.request_options.chat_request.stream_thinking = enable;
        self
    }

    /// 设置单轮推理 Token 上限。
    ///
    /// 与 `max_output_tokens` 分离：thinking 是模型内部推理，不计入输出预算。
    /// 透传给 Provider 层，由 Adapter 映射为协议特定字段。
    pub fn reasoning_budget(mut self, max: u32) -> Self {
        self.config
            .request_options
            .chat_request
            .max_reasoning_tokens = Some(max);
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
