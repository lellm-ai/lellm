//! LLM 调用节点 — 执行单次 LLM 调用。
//!
//! **职责单一：** 只负责"调用 LLM + 收集流式响应 + emit Effects"。
//! 不感知 Budget、Compaction、Iteration Limit 等运行时策略。
//!
//! # 分层
//!
//! ```text
//! LLMNode  — 只负责 State ↔ Request ↔ Effects
//!    │
//! LlmInvoker — 只负责防御策略（retry, fallback, stream state machine）
//!    │
//! LlmProvider — 只负责 protocol adapter（stateless）
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;

use lellm_core::{ChatResponse, ContentBlock, Message, TextBlock, ThinkingBlock};
use lellm_graph::{GraphError, LeafContext, LeafNode, TerminalError};

use super::super::config::{ToolUseConfig, build_request_inner_with_round};
use super::super::context::{estimate_reasoning_block, estimate_text};
use super::super::invoker::LlmInvoker;
use super::super::runtime::ResolvedRound;
use super::super::stream_translation::{TranslationResult, translate_provider_event};
use super::super::tools::ToolExecutor;
use super::super::typed_state::{AgentMutation, AgentState};

/// 分离 output / reasoning token
fn split_output_tokens(content: &[lellm_core::ContentBlock]) -> (usize, usize) {
    let mut output_tokens: usize = 0;
    let mut reasoning_tokens: usize = 0;
    for b in content {
        match b {
            lellm_core::ContentBlock::Text(t) => output_tokens += estimate_text(&t.text),
            lellm_core::ContentBlock::Thinking(th) => {
                reasoning_tokens += estimate_reasoning_block(th)
            }
            lellm_core::ContentBlock::Image { .. } | lellm_core::ContentBlock::ToolCall(_) => {}
        }
    }
    (output_tokens, reasoning_tokens)
}

/// LLM 调用节点。
///
/// # Typed State
///
/// 从 ctx 获取 `AgentState`，直接操作 typed 字段，写回 ctx。
#[derive(Clone)]
pub struct LLMNode {
    pub name: String,
    /// LLM 调用器 — 封装 retry/fallback/stream state machine
    pub invoker: Arc<LlmInvoker>,
    /// 工具执行器 — 用于获取工具定义
    pub executor: ToolExecutor,
    /// 配置 — 用于构建请求
    pub config: ToolUseConfig,
}

impl LLMNode {
    pub fn new(
        name: impl Into<String>,
        invoker: Arc<LlmInvoker>,
        executor: ToolExecutor,
        config: ToolUseConfig,
    ) -> Self {
        Self {
            name: name.into(),
            invoker,
            executor,
            config,
        }
    }
}

#[async_trait]
impl LeafNode<AgentState> for LLMNode {
    async fn execute(&self, ctx: &mut LeafContext<'_, AgentState>) -> Result<(), GraphError> {
        // 1. 获取 AgentState
        let state = ctx.state().clone();

        // 2. Emit 迭代递增 Mutation
        ctx.record(AgentMutation::IncrementIteration);

        // 3. 获取工具定义 & 构建 LLM 请求
        let round = ResolvedRound::new(self.executor.snapshot().await);

        let req = build_request_inner_with_round(
            self.invoker.model(),
            &state.messages,
            self.config.max_output_tokens,
            &self.config.request_options,
            state.iterations + 1,
            &round.definitions,
            self.config.tool_cache_policy,
        );

        // 4. 通过 LlmInvoker 执行流式调用（自动处理 retry/fallback）
        let mut stream = self
            .invoker
            .invoke_stream(&req, &state.messages, state.iterations)
            .await
            .map_err(|e| {
                GraphError::Terminal(TerminalError::NodeExecutionFailed {
                    node: self.name.clone(),
                    source: e.into(),
                })
            })?;

        // 收集流式事件，构建完整的 ChatResponse
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut current_text = String::new();
        let mut current_thinking = String::new();
        let mut tool_calls_count: usize = 0;
        let mut usage: Option<lellm_core::TokenUsage> = None;

        while let Some(event) = stream.next().await {
            match event {
                Ok(provider_event) => {
                    // ResponseComplete 需要提取 tool_calls（所有权转移），单独处理
                    if let lellm_provider::ProviderEvent::ResponseComplete {
                        tool_calls,
                        usage: u,
                    } = &provider_event
                    {
                        for tc in tool_calls {
                            content_blocks.push(ContentBlock::ToolCall(tc.clone()));
                        }
                        tool_calls_count = content_blocks
                            .iter()
                            .filter(|b| matches!(b, ContentBlock::ToolCall(_)))
                            .count();
                        usage = u.clone();
                        continue;
                    }

                    match translate_provider_event(&provider_event) {
                        TranslationResult::EmitWithText { chunk, delta } => {
                            ctx.emit(chunk);
                            current_text.push_str(&delta);
                        }
                        TranslationResult::EmitWithThinking { chunk, delta, .. } => {
                            ctx.emit(chunk);
                            current_thinking.push_str(&delta);
                        }
                        TranslationResult::Emit(chunk) => ctx.emit(chunk),
                        _ => {}
                    }
                }
                Err(e) => {
                    return Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                        node: self.name.clone(),
                        source: e.into(),
                    }));
                }
            }
        }

        // 构建完整的 ChatResponse
        if !current_thinking.is_empty() {
            content_blocks.push(ContentBlock::Thinking(ThinkingBlock {
                thinking: current_thinking,
                redacted: None,
            }));
        }
        if !current_text.is_empty() {
            content_blocks.push(ContentBlock::Text(TextBlock {
                text: current_text,
                cache_control: None,
            }));
        }

        let response = ChatResponse {
            content: content_blocks,
            usage: usage.unwrap_or_default(),
            raw: serde_json::json!(null),
        };

        // 5. 分离 output / reasoning token，Emit Token Effects
        let (output_tokens, reasoning_tokens) = split_output_tokens(&response.content);
        ctx.record(AgentMutation::AddOutputTokens(output_tokens));
        ctx.record(AgentMutation::AddReasoningTokens(reasoning_tokens));

        // 6. Emit 消息追加 Mutation
        let content = response.content.clone();
        let msg = Message::Assistant { content };
        ctx.record(AgentMutation::AppendMessage(msg));

        // 7. 记录 tool_calls
        let has_tools = response.has_tool_calls();
        if has_tools {
            ctx.record(AgentMutation::AddToolCalls(tool_calls_count));
        }

        // 8. Emit LastResponse（供 PostLLMGuard 检查）
        ctx.record(AgentMutation::SetLastResponse(response));

        // 路由决策全部交给 PostLLMGuard，此处不调用 ctx.goto()/ctx.end()
        tracing::debug!(
            iteration = state.iterations + 1,
            has_tool_calls = has_tools,
            "LLM call completed"
        );

        Ok(())
    }
}
