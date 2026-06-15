//! RequestOptions — Agent 每轮 LLM 调用的生成参数。
//!
//! 独立字段定义，不与 `ChatRequest` 耦合。
//! `apply()` 方法将非默认值覆盖到基础 `ChatRequest` 上。
//!
//! # 设计原则
//!
//! - **独立字段**：不包裹 `ChatRequest`，避免不必要的耦合
//! - **选择性覆盖**：`apply()` 只覆盖非默认值，保留基础请求的核心字段
//! - **Agent 保留字段**：`model`、`messages`、`tools` 由 Agent 层注入，`apply()` 跳过

use lellm_core::ChatRequest;

/// Agent 每轮 LLM 调用的生成参数覆盖。
///
/// 用户只需设置需要覆盖的字段。
/// 未设置的字段保持默认值（None），`apply()` 时会被跳过。
///
/// # Agent 保留字段
///
/// 以下字段由 Agent 层自动注入，`apply()` 不会覆盖：
/// - `model` — 来自 `ResolvedModel`
/// - `messages` — 来自对话历史
/// - `tools` — 来自 `ToolExecutor`
///
/// # 示例
///
/// ```ignore
/// let opts = RequestOptions::new()
///     .temperature(0.1)
///     .reasoning(ReasoningConfig::High);
///
/// let agent = AgentBuilder::new(model)
///     .request_options(opts)
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<u64>,
    pub tool_choice: Option<lellm_core::ToolChoice>,
    pub stop_sequences: Option<Vec<String>>,
    pub prefill: Option<String>,
    pub reasoning: Option<lellm_core::ReasoningConfig>,
    pub max_reasoning_tokens: Option<u32>,
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

impl RequestOptions {
    /// 创建空的 RequestOptions（所有字段为默认值）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 将非默认值覆盖到基础 ChatRequest 上。
    ///
    /// **跳过字段：** `model`、`messages`、`tools`（由 Agent 层注入）
    /// **跳过规则：** `None`、空 String、空 Vec、false 等默认值不覆盖
    pub fn apply(&self, req: &mut ChatRequest) {
        if self.temperature.is_some() {
            req.temperature = self.temperature;
        }
        if self.top_p.is_some() {
            req.top_p = self.top_p;
        }
        if self.seed.is_some() {
            req.seed = self.seed;
        }
        if self.tool_choice.is_some() {
            req.tool_choice = self.tool_choice.clone();
        }
        if self.stop_sequences.is_some() {
            req.stop_sequences = self.stop_sequences.clone();
        }
        if self.prefill.is_some() {
            req.prefill = self.prefill.clone();
        }
        if self.reasoning.is_some() {
            req.reasoning = self.reasoning;
        }
        if self.max_reasoning_tokens.is_some() {
            req.max_reasoning_tokens = self.max_reasoning_tokens;
        }
        if self.extra.is_some() {
            req.extra = self.extra.clone();
        }
    }
}

// ─── 链式 setter ──────────────────────────────────────────────────

impl RequestOptions {
    pub fn temperature(mut self, t: f64) -> Self {
        self.temperature = Some(t);
        self
    }

    pub fn top_p(mut self, p: f64) -> Self {
        self.top_p = Some(p);
        self
    }

    pub fn seed(mut self, s: u64) -> Self {
        self.seed = Some(s);
        self
    }

    pub fn tool_choice(mut self, choice: lellm_core::ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    pub fn stop_sequences(mut self, seqs: Vec<String>) -> Self {
        self.stop_sequences = Some(seqs);
        self
    }

    pub fn prefill(mut self, text: String) -> Self {
        self.prefill = Some(text);
        self
    }

    pub fn reasoning(mut self, r: lellm_core::ReasoningConfig) -> Self {
        self.reasoning = Some(r);
        self
    }

    pub fn max_reasoning_tokens(mut self, max: u32) -> Self {
        self.max_reasoning_tokens = Some(max);
        self
    }

    pub fn extra(mut self, extra: serde_json::Map<String, serde_json::Value>) -> Self {
        self.extra = Some(extra);
        self
    }
}
