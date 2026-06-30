//! ToolUseLoop 配置与请求构建辅助。
//!
//! - `ToolUseConfig` — 纯参数，Clone + Send + Sync
//! - `ToolUseDeps` — 策略服务，Arc 包裹
//! - `build_request_*` — 请求构建辅助函数

use lellm_core::{CacheControl, ChatRequest, LlmError, Message, Prompt, ToolDefinition};
use lellm_provider::ResolvedModel;

use super::context::ContextBudget;
use super::fallback::FallbackStrategy;
use super::request_opts::RequestOptions;
use super::retry::RetryPolicy;
use std::sync::Arc;

// ─── 配置（纯参数）──────────────────────────────────────────────

/// ToolUseLoop 纯参数配置。
///
/// - `Clone` + `Send` + `Sync` — 可安全跨线程共享
/// - 仅包含数据字段，不含行为逻辑
/// - 未来可扩展为 `Serialize` / `Deserialize`
#[derive(Debug, Clone)]
pub struct ToolUseConfig {
    /// 系统提示（运行时注入，不修改 messages）。
    ///
    /// 支持 `Prompt` 类型，统一了简单文本与分层缓存两种模式。
    /// - 简单文本：通过 `From<String>` 自动转换
    /// - 分层缓存：`Prompt::builder().layer_cached(...).build()`
    pub system: Option<Prompt>,
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
    /// 整个 Agent Run 的最大推理 token 总数（可选，默认无限制）。
    ///
    /// 与 `max_total_output_tokens` 分离：thinking 是模型内部推理，不计入输出预算。
    /// 双层设计：
    /// - 单轮：`RequestOptions.max_reasoning_tokens` → 透传给 Provider
    /// - 总计：`max_total_reasoning_tokens` → Agent 层累计检查
    pub max_total_reasoning_tokens: Option<u32>,
    /// 上下文预算管理（默认开启）
    ///
    /// **v0.1**: 默认 `ContextBudget::default()`（max_tokens = 128,000）
    /// **v0.2**: 从 `ResolvedModel.context_window` 自动推导（window * 0.8）
    ///
    /// 若要关闭限制，设置 `max_tokens = usize::MAX`。
    pub context_budget: ContextBudget,
    /// 每轮 LLM 调用的生成参数覆盖。
    ///
    /// 独立字段定义，不与 `ChatRequest` 耦合。
    /// `apply()` 方法将非默认值（temperature、top_p、reasoning 等）
    /// 覆盖到 Agent 层构建的基础 `ChatRequest` 上。
    ///
    /// `model`、`messages`、`tools` 由 Agent 层注入，不会被覆盖。
    pub request_options: RequestOptions,
    /// 是否向消费者流式输出推理过程（ThinkingDelta 事件）。
    ///
    /// `false`（默认）= 模型可推理，但不向消费者发射 ThinkingDelta 事件
    /// `true` = 将推理内容以 ThinkingDelta 事件流式输出
    ///
    /// **重要：** 此字段控制框架行为（Event 管道），不属于协议参数。
    /// 不应出现在 `ChatRequest` 中（Codec 不应看到此字段）。
    pub stream_thinking: bool,
    /// 工具缓存策略（默认 `Auto`）。
    ///
    /// 控制框架如何为 Tool Definitions 添加 `cache_control` 标记：
    /// - `Auto`（默认）：为未设置 `cache_control` 的工具自动添加 `Breakpoint`
    /// - `Preserve`：不修改用户设置的 `cache_control`
    /// - `Disabled`：显式清除所有工具的 `cache_control`
    pub tool_cache_policy: ToolCachePolicy,
    /// 工具重试策略
    pub retry_policy: RetryPolicy,
}

/// 工具缓存策略。
///
/// 控制框架如何为 Tool Definitions 补充 `cache_control` 标记，
/// 以最大化前缀缓存命中率。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolCachePolicy {
    /// 自动模式（默认）：为未设置 `cache_control` 的工具添加 `Breakpoint`。
    /// 已显式设置的标记不会被覆盖。
    #[default]
    Auto,
    /// 保留模式：不修改用户设置的 `cache_control`。
    Preserve,
    /// 禁用模式：清除所有工具的 `cache_control`。
    Disabled,
}

impl Default for ToolUseConfig {
    fn default() -> Self {
        Self {
            system: None,
            max_iterations: 10,
            max_output_tokens: 4_000,
            max_total_output_tokens: None,
            max_total_reasoning_tokens: None,
            context_budget: ContextBudget::default(),
            request_options: RequestOptions::default(),
            stream_thinking: false,
            tool_cache_policy: ToolCachePolicy::default(),
            retry_policy: RetryPolicy::default(),
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
    if let Some(ref system) = config.system {
        if has_system_message(messages) {
            return Err(LlmError::DuplicateSystemPrompt);
        }
        let mut result = vec![Message::System {
            content: system.to_content_blocks(),
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
///
/// `definitions` — 预解析的工具定义列表（从 ResolvedRound 获取）。
/// `tool_cache_policy` — 工具缓存策略，控制如何为 tools 添加 cache_control。
pub(super) fn build_request_inner(
    model: &ResolvedModel,
    messages: &[Message],
    max_output_tokens: u32,
    request_options: &RequestOptions,
    definitions: &[ToolDefinition],
    tool_cache_policy: ToolCachePolicy,
) -> ChatRequest {
    let tools = match tool_cache_policy {
        ToolCachePolicy::Auto => {
            if definitions.is_empty() {
                None
            } else {
                Some(
                    definitions
                        .iter()
                        .map(|d| {
                            if d.cache_control.is_none() {
                                let mut cloned = d.clone();
                                cloned.cache_control = Some(CacheControl::Breakpoint);
                                cloned
                            } else {
                                d.clone()
                            }
                        })
                        .collect(),
                )
            }
        }
        ToolCachePolicy::Preserve => {
            if definitions.is_empty() {
                None
            } else {
                Some(definitions.to_vec())
            }
        }
        ToolCachePolicy::Disabled => {
            if definitions.is_empty() {
                None
            } else {
                Some(
                    definitions
                        .iter()
                        .map(|d| {
                            let mut cloned = d.clone();
                            cloned.cache_control = None;
                            cloned
                        })
                        .collect(),
                )
            }
        }
    };

    let mut req = ChatRequest {
        model: model.model.clone(),
        messages: messages.to_vec(),
        tools,
        max_tokens: Some(max_output_tokens),
        temperature: None,
        top_p: None,
        seed: None,
        tool_choice: None,
        stop_sequences: None,
        prefill: None,
        reasoning: None,
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
///
/// `definitions` — 预解析的工具定义列表（从 ResolvedRound 获取）。
/// `tool_cache_policy` — 工具缓存策略，控制如何为 tools 添加 cache_control。
pub(super) fn build_request_inner_with_round(
    model: &ResolvedModel,
    messages: &[Message],
    max_output_tokens: u32,
    request_options: &RequestOptions,
    iteration: usize,
    definitions: &[ToolDefinition],
    tool_cache_policy: ToolCachePolicy,
) -> ChatRequest {
    let mut req = build_request_inner(
        model,
        messages,
        max_output_tokens,
        request_options,
        definitions,
        tool_cache_policy,
    );

    // 如果 RequestOptions 设置了 tool_choice 且不是第一轮，清除它
    // 让 LLM 在工具调用后自主选择
    if iteration > 0 && request_options.tool_choice.is_some() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use lellm_core::Prompt;

    #[test]
    fn test_build_request_messages_with_prompt() {
        let config = ToolUseConfig {
            system: Some(
                Prompt::builder()
                    .layer_cached("核心身份")
                    .layer_cached("工具指南")
                    .layer_dynamic("会话上下文")
                    .build(),
            ),
            ..Default::default()
        };

        let messages: Vec<Message> = vec![Message::user_text("你好")];
        let result = build_request_messages_inner(&config, &messages).unwrap();

        // Should have system + user
        assert_eq!(result.len(), 2);

        // Verify system message has 3 content blocks with correct cache markers.
        // Only the LAST cached layer gets the breakpoint (Anthropic max 4 per request).
        if let Message::System { content } = &result[0] {
            assert_eq!(content.len(), 3);

            // Layer 1 — cached, but NO breakpoint (not the last cached)
            if let lellm_core::ContentBlock::Text(t) = &content[0] {
                assert_eq!(t.text, "核心身份");
                assert!(
                    t.cache_control.is_none(),
                    "Intermediate cached layer should NOT have breakpoint"
                );
            } else {
                panic!("expected Text block");
            }

            // Layer 2 — cached, HAS breakpoint (last cached before dynamic)
            if let lellm_core::ContentBlock::Text(t) = &content[1] {
                assert_eq!(t.text, "工具指南");
                assert!(
                    t.cache_control.is_some(),
                    "Last cached layer should have breakpoint"
                );
            } else {
                panic!("expected Text block");
            }

            // Layer 3 — dynamic (no cache)
            if let lellm_core::ContentBlock::Text(t) = &content[2] {
                assert_eq!(t.text, "会话上下文");
                assert!(t.cache_control.is_none());
            } else {
                panic!("expected Text block");
            }
        } else {
            panic!("expected System message");
        }
    }

    #[test]
    fn test_build_request_messages_with_plain_prompt() {
        let config = ToolUseConfig {
            system: Some("简单提示".into()),
            ..Default::default()
        };

        let messages: Vec<Message> = vec![Message::user_text("你好")];
        let result = build_request_messages_inner(&config, &messages).unwrap();

        assert_eq!(result.len(), 2);
        if let Message::System { content } = &result[0] {
            assert_eq!(content.len(), 1);
            assert_eq!(content[0].as_text(), Some("简单提示"));
        } else {
            panic!("expected System message");
        }
    }

    #[test]
    fn test_build_request_messages_no_system() {
        let config = ToolUseConfig::default();
        let messages: Vec<Message> = vec![Message::user_text("你好")];
        let result = build_request_messages_inner(&config, &messages).unwrap();

        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Message::User { .. }));
    }

    #[test]
    fn test_duplicate_system_prompt_error() {
        let config = ToolUseConfig {
            system: Some("系统提示".into()),
            ..Default::default()
        };

        let messages: Vec<Message> =
            vec![Message::system_text("已有系统"), Message::user_text("你好")];
        let result = build_request_messages_inner(&config, &messages);

        assert!(result.is_err());
    }
}
