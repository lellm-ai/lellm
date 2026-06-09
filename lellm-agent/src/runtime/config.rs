//! ToolUseLoop 配置与请求构建辅助。
//!
//! - `ToolUseConfig` — 纯参数，Clone + Send + Sync
//! - `ToolUseDeps` — 策略服务，Arc 包裹
//! - `build_request_*` — 请求构建辅助函数

use lellm_core::{ChatRequest, LlmError, Message};
use lellm_provider::ResolvedModel;

use super::context::ContextBudget;
use super::fallback::FallbackStrategy;
use super::request_opts::RequestOptions;
use super::tools::ToolExecutor;
use std::sync::Arc;

// ─── 配置（纯参数）──────────────────────────────────────────────

/// ToolUseLoop 纯参数配置。
///
/// - `Clone` + `Send` + `Sync` — 可安全跨线程共享
/// - 仅包含数据字段，不含行为逻辑
/// - 未来可扩展为 `Serialize` / `Deserialize`
#[derive(Debug, Clone)]
pub struct ToolUseConfig {
    /// 系统提示（运行时注入，不修改 messages）
    pub system_prompt: Option<String>,
    /// 最大迭代轮次（默认 10）
    pub max_iterations: usize,
    /// 每次 LLM 请求的最大输出 token 数（默认 4k）
    ///
    /// 控制单次 Provider 调用的响应长度上限，防止模型输出过长。
    /// 工具调用轮次通常只需几百 token，但模型 thinking 会消耗额外空间，
    /// 4k 在"够用"和"不浪费"之间取得平衡。
    /// 若需要长文本生成，可通过 Builder 调大。
    /// 会自动注入到 `ChatRequest.max_tokens`。
    pub max_output_tokens: u32,
    /// 整个 Agent Run 的最大输出 token 总数（可选，默认无限制）。
    ///
    /// 即使每轮的 `max_output_tokens` 设置合理，多轮工具调用仍可能导致
    /// 总输出巨大（如 10 轮 × 4k = 40k）。此字段提供聚合层面的保险丝，
    /// 防止因工具循环或 Provider 忽略 max_tokens 而导致的成本失控。
    ///
    /// 统计范围：Assistant Text（不含 Thinking，不含 Tool Call 结构开销）。
    /// 在流式模式下边接收边检查，达到阈值立即停止。
    pub max_total_output_tokens: Option<u32>,
    /// 上下文预算管理（默认开启）
    ///
    /// **v0.1**: 默认 `ContextBudget::default()`（max_tokens = 128,000）
    /// **v0.2**: 从 `ResolvedModel.context_window` 自动推导（window * 0.8）
    ///
    /// 若要关闭限制，设置 `max_tokens = usize::MAX`。
    pub context_budget: ContextBudget,
    /// 每轮 LLM 调用的生成参数覆盖。
    ///
    /// 内部包裹 `ChatRequest`，与 core 层零重复定义。
    /// `apply()` 方法将非默认值（temperature、top_p、reasoning 等）
    /// 覆盖到 Agent 层构建的基础 `ChatRequest` 上。
    ///
    /// `model`、`messages`、`tools` 由 Agent 层注入，不会被覆盖。
    pub request_options: RequestOptions,
}

impl Default for ToolUseConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_iterations: 10,
            max_output_tokens: 4_000,
            max_total_output_tokens: None,
            context_budget: ContextBudget::default(),
            request_options: RequestOptions::default(),
        }
    }
}

// ─── 依赖（策略服务）────────────────────────────────────────────

/// ToolUseLoop 策略依赖。
///
/// 包含有行为逻辑的服务对象（Arc 包裹），与纯参数 Config 分离。
#[derive(Clone)]
pub struct ToolUseDeps {
    /// Provider 降级策略
    pub fallback: Arc<dyn FallbackStrategy>,
}

impl Default for ToolUseDeps {
    fn default() -> Self {
        Self {
            fallback: Arc::new(super::fallback::DefaultFallback::default()),
        }
    }
}

// ─── 辅助函数 ───────────────────────────────────────────────────

/// 检查消息列表中是否已存在 System 消息。
pub(super) fn has_system_message(messages: &[Message]) -> bool {
    messages.iter().any(|m| matches!(m, Message::System { .. }))
}

/// 构建有效的请求消息列表（用于 spawned task，无法使用 &self）
pub(super) fn build_request_messages_inner(
    config: &ToolUseConfig,
    messages: &[Message],
) -> Result<Vec<Message>, LlmError> {
    if let Some(ref sp) = config.system_prompt {
        if has_system_message(messages) {
            return Err(LlmError::DuplicateSystemPrompt);
        }
        let mut result = vec![Message::System {
            content: lellm_core::text_block(sp.clone()),
        }];
        result.extend(messages.iter().cloned());
        Ok(result)
    } else {
        Ok(messages.to_vec())
    }
}

/// 构建 ChatRequest（用于 spawned task）
///
/// 先构建基础请求（Agent 层注入 model/messages/tools/max_tokens），
/// 再应用 RequestOptions 非默认值覆盖。
pub(super) fn build_request_inner(
    model: &ResolvedModel,
    executor: &ToolExecutor,
    messages: &[Message],
    max_output_tokens: u32,
    request_options: &RequestOptions,
) -> ChatRequest {
    let mut req = ChatRequest {
        model: model.model.clone(),
        messages: messages.to_vec(),
        tools: executor.has_tools().then(|| executor.definitions()),
        max_tokens: Some(max_output_tokens),
        temperature: None,
        top_p: None,
        seed: None,
        tool_choice: None,
        stop_sequences: None,
        prefill: None,
        reasoning: None,
        stream_thinking: false,
        max_reasoning_tokens: None,
        extra: None,
    };

    // 应用 RequestOptions 非默认值覆盖
    request_options.apply(&mut req);

    req
}

/// 构建首轮 ChatRequest，支持强制指定工具（仅第一轮生效）。
///
/// 当 `RequestOptions` 设置了 `tool_choice` 时，仅在第一轮注入；
/// 后续轮次由 LLM 自主决定是否调用工具。
pub(super) fn build_request_inner_with_round(
    model: &ResolvedModel,
    executor: &ToolExecutor,
    messages: &[Message],
    max_output_tokens: u32,
    request_options: &RequestOptions,
    iteration: usize,
) -> ChatRequest {
    let mut req = build_request_inner(
        model,
        executor,
        messages,
        max_output_tokens,
        request_options,
    );

    // 如果 RequestOptions 设置了 tool_choice 且不是第一轮，清除它
    // 让 LLM 在工具调用后自主选择
    if iteration > 0 && request_options.chat_request.tool_choice.is_some() {
        req.tool_choice = None;
    }

    req
}

/// 构建空的 ChatResponse（边界情况兜底）
pub(super) fn empty_response() -> lellm_core::ChatResponse {
    lellm_core::ChatResponse::new(
        lellm_core::text_block(String::new()),
        lellm_core::TokenUsage::default(),
        serde_json::Value::Null,
    )
}
