//! RequestOptions — Agent 每轮 LLM 调用的生成参数。
//!
//! 内部包裹一个 `ChatRequest`，与 core 层零重复定义，彻底消除同步风险。
//! `apply()` 方法将非默认值覆盖到基础 `ChatRequest` 上。
//!
//! # 设计原则
//!
//! - **零重复**：不重新定义字段，直接包裹 `ChatRequest`
//! - **选择性覆盖**：`apply()` 只覆盖非默认值，保留基础请求的核心字段
//! - **Agent 保留字段**：`model`、`messages`、`tools` 由 Agent 层注入，`apply()` 跳过

use lellm_core::ChatRequest;

/// Agent 每轮 LLM 调用的生成参数覆盖。
///
/// 内部持有一个 `ChatRequest`，用户只需设置需要覆盖的字段。
/// 未设置的字段保持默认值，`apply()` 时会被跳过。
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
#[derive(Debug, Clone)]
pub struct RequestOptions {
    pub chat_request: ChatRequest,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            chat_request: ChatRequest::default(),
        }
    }
}

impl RequestOptions {
    /// 创建空的 RequestOptions（所有字段为默认值）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 ChatRequest 直接构造。
    pub fn from_request(req: ChatRequest) -> Self {
        Self { chat_request: req }
    }

    /// 将非默认值覆盖到基础 ChatRequest 上。
    ///
    /// **跳过字段：** `model`、`messages`、`tools`（由 Agent 层注入）
    /// **跳过规则：** `None`、空 String、空 Vec、false 等默认值不覆盖
    pub fn apply(&self, req: &mut ChatRequest) {
        let o = &self.chat_request;

        // 生成参数 — 非默认才覆盖
        if o.temperature.is_some() {
            req.temperature = o.temperature;
        }
        if o.top_p.is_some() {
            req.top_p = o.top_p;
        }
        if o.seed.is_some() {
            req.seed = o.seed;
        }
        if o.tool_choice.is_some() {
            req.tool_choice = o.tool_choice.clone();
        }
        if o.stop_sequences.is_some() {
            req.stop_sequences = o.stop_sequences.clone();
        }
        if o.prefill.is_some() {
            req.prefill = o.prefill.clone();
        }
        if o.reasoning.is_some() {
            req.reasoning = o.reasoning;
        }
        if o.max_reasoning_tokens.is_some() {
            req.max_reasoning_tokens = o.max_reasoning_tokens;
        }
        if o.extra.is_some() {
            req.extra = o.extra.clone();
        }

        // max_tokens 也覆盖（但 build_request_inner 会用自己的 max_output_tokens 再次设置）
        if o.max_tokens.is_some() {
            // 注意：Agent 层的 max_output_tokens 优先级更高
            // 这里不覆盖，让 build_request_inner 决定
        }
    }
}

// ─── 链式 setter — 直接操作内部 ChatRequest ────────────────────

impl RequestOptions {
    pub fn temperature(mut self, t: f64) -> Self {
        self.chat_request.temperature = Some(t);
        self
    }

    pub fn top_p(mut self, p: f64) -> Self {
        self.chat_request.top_p = Some(p);
        self
    }

    pub fn seed(mut self, s: u64) -> Self {
        self.chat_request.seed = Some(s);
        self
    }

    pub fn tool_choice(mut self, choice: lellm_core::ToolChoice) -> Self {
        self.chat_request.tool_choice = Some(choice);
        self
    }

    pub fn stop_sequences(mut self, seqs: Vec<String>) -> Self {
        self.chat_request.stop_sequences = Some(seqs);
        self
    }

    pub fn prefill(mut self, text: String) -> Self {
        self.chat_request.prefill = Some(text);
        self
    }

    pub fn reasoning(mut self, r: lellm_core::ReasoningConfig) -> Self {
        self.chat_request.reasoning = Some(r);
        self
    }

    pub fn max_reasoning_tokens(mut self, max: u32) -> Self {
        self.chat_request.max_reasoning_tokens = Some(max);
        self
    }

    pub fn extra(mut self, extra: serde_json::Map<String, serde_json::Value>) -> Self {
        self.chat_request.extra = Some(extra);
        self
    }
}
