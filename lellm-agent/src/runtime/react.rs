//! ReAct Graph — ToolUseLoop 内部构建的有环图。
//!
//! v04 设计：ToolUseLoop 内部不再手写 while 循环，
//! 构建内部 Graph（LLM Node → Condition → Tool Node → 自环），
//! 调用 `Graph::run_inline()` 驱动循环。
//!
//! v0.4+ Typed State: 节点使用 `AgentState` 替代 `HashMap<String, Value>`，
//! 通过 `AgentEffect` 描述状态转换。
//!
//! ```text
//! [LLM] --有tool_calls--> [Tool] --(自环)--> [LLM]
//!      --无tool_calls--> [End]
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;

use lellm_core::{ChatResponse, ContentBlock, Message, TextBlock, ThinkingBlock, ToolCall};
use lellm_graph::{
    FlowNode, Graph, GraphBuilder, GraphError, NodeContext, NodeKind, StateEffect, TaskNode,
    TerminalError,
};
use lellm_provider::ProviderEvent;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_inner_with_round, empty_response};
use super::context::{
    AgentExecutionContext, ContextBudget, ContextCompactor, LocalCompactor,
    estimate_reasoning_block, estimate_text,
};
use super::event::StopReason;
use super::runtime::ResolvedRound;
use super::tools::{ToolExecutor, execute_batch_with};
use super::typed_state::{AGENT_STATE_KEY, AgentState};
use lellm_provider::ResolvedModel;

// ─── State Keys（边条件等仍需要字符串 key）──────────────────

pub(crate) const SK_MESSAGES: &str = "messages";
pub(crate) const SK_ITERATIONS: &str = "iterations";
pub(crate) const SK_TOTAL_TOOL_CALLS: &str = "total_tool_calls";
pub(crate) const SK_OUTPUT_TOKENS: &str = "output_tokens";
pub(crate) const SK_REASONING_TOKENS: &str = "reasoning_tokens";
pub(crate) const SK_HAS_TOOL_CALLS: &str = "has_tool_calls";
pub(crate) const SK_STOP_REASON: &str = "stop_reason";
pub(crate) const SK_LAST_RESPONSE: &str = "last_response";
pub(crate) const SK_COMPACT_COUNT: &str = "compact_count";

// ─── Typed State 辅助函数 ──────────────────────────────────────

/// 从 NodeContext 的 Typed State 中获取 AgentState。不存在则创建空状态。
fn get_agent_state(ctx: &NodeContext<'_>) -> AgentState {
    ctx.state()
        .get(AGENT_STATE_KEY)
        .and_then(|v| AgentState::from_value(v.clone()))
        .unwrap_or_default()
}

/// Effect Only：节点只 emit_effect，不直接写 State。
/// Executor / run_inline 消费 Effects → apply 到 Typed State。
fn emit_effect(ctx: &mut NodeContext<'_>, effect: super::typed_state::AgentEffect) {
    ctx.emit_effect(effect);
}

/// 将 AgentState 关键字段以 StateEffect 写入 State（HashMap），
/// 供边条件（edge_if）读取。边条件闭包接收 &State，无法直接读 AgentState。
fn emit_state_bridge(ctx: &mut NodeContext<'_>, state: &AgentState) {
    ctx.emit_effect(StateEffect::Put(
        SK_ITERATIONS.into(),
        serde_json::json!(state.iterations),
    ));
    ctx.emit_effect(StateEffect::Put(
        SK_OUTPUT_TOKENS.into(),
        serde_json::json!(state.output_tokens),
    ));
    ctx.emit_effect(StateEffect::Put(
        SK_REASONING_TOKENS.into(),
        serde_json::json!(state.reasoning_tokens),
    ));
    ctx.emit_effect(StateEffect::Put(
        SK_TOTAL_TOOL_CALLS.into(),
        serde_json::json!(state.total_tool_calls),
    ));
    ctx.emit_effect(StateEffect::Put(
        SK_COMPACT_COUNT.into(),
        serde_json::json!(state.compact_count),
    ));
    ctx.emit_effect(StateEffect::Put(
        SK_LAST_RESPONSE.into(),
        serde_json::json!(state.last_response),
    ));
}

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

// ─── LLMNode ──────────────────────────────────────────────────

/// LLM 调用节点 — 执行单次 LLM 调用。
///
/// 职责单一：只负责 LLM 调用，不感知 Compaction。
///
/// # Typed State
///
/// 从 ctx 获取 `AgentState`，直接操作 typed 字段，写回 ctx。
pub struct LLMNode {
    pub name: String,
    pub model: ResolvedModel,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
    pub deps: ToolUseDeps,
}

impl Clone for LLMNode {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            model: self.model.clone(),
            executor: self.executor.clone(),
            config: self.config.clone(),
            deps: self.deps.clone(),
        }
    }
}

impl LLMNode {
    pub fn new(
        name: impl Into<String>,
        model: ResolvedModel,
        executor: ToolExecutor,
        config: ToolUseConfig,
        deps: ToolUseDeps,
    ) -> Self {
        Self {
            name: name.into(),
            model,
            executor,
            config,
            deps,
        }
    }
}

#[async_trait]
impl FlowNode for LLMNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        use super::typed_state::AgentEffect;

        // 1. 获取 AgentState（只读）
        let state = get_agent_state(ctx);
        let mut exec_ctx = AgentExecutionContext::new(state.messages_ref());

        // 2. 检查最大迭代
        if state.reached_max(self.config.max_iterations) {
            emit_effect(ctx, AgentEffect::SetStopReason(StopReason::MaxIterationsReached));
            let last_response = state.last_response.clone().unwrap_or_else(empty_response);
            emit_effect(ctx, AgentEffect::SetLastResponse(last_response));
            emit_state_bridge(ctx, &state);
            ctx.end();
            return Ok(());
        }

        // 3. Emit 迭代递增 Effect
        emit_effect(ctx, AgentEffect::IncrementIteration);

        // 4. 获取工具定义
        let round = ResolvedRound::new(self.executor.snapshot().await);

        // 5. 构建 LLM 请求（使用新迭代数）
        let req = build_request_inner_with_round(
            &self.model,
            &state.messages,
            self.config.max_output_tokens,
            &self.config.request_options,
            state.iterations + 1,
            &round.definitions,
        );

        // 6. 执行 LLM 流式调用（v04: 真正的流式输出）
        let mut stream = self.model.provider.stream(&req).await.map_err(|e| {
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
                Ok(ProviderEvent::Token { token }) => {
                    ctx.emit(lellm_graph::StreamChunk::Text(token.clone()));
                    current_text.push_str(&token);
                }
                Ok(ProviderEvent::ThinkingDelta { thinking, .. }) => {
                    ctx.emit(lellm_graph::StreamChunk::Thinking(thinking.clone()));
                    current_thinking.push_str(&thinking);
                }
                Ok(ProviderEvent::ResponseComplete {
                    tool_calls,
                    usage: u,
                }) => {
                    for tc in tool_calls {
                        content_blocks.push(ContentBlock::ToolCall(tc));
                    }
                    tool_calls_count = content_blocks
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolCall(_)))
                        .count();
                    usage = u;
                }
                Ok(_) => {}
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

        // 7. 分离 output / reasoning token
        let (output_tokens, reasoning_tokens) = split_output_tokens(&response.content);
        exec_ctx.add_tokens(output_tokens + reasoning_tokens);

        // 8. 检查 reasoning budget（单轮）
        if let Some(limit) = self.config.request_options.max_reasoning_tokens {
            let round_reasoning: usize = response
                .content
                .iter()
                .filter_map(|b| match b {
                    lellm_core::ContentBlock::Thinking(th) => Some(estimate_reasoning_block(th)),
                    _ => None,
                })
                .sum();
            if round_reasoning > limit as usize {
                tracing::warn!(
                    round_reasoning,
                    max_reasoning_tokens = limit,
                    "single-round reasoning budget exceeded"
                );
                emit_effect(ctx, AgentEffect::AddOutputTokens(output_tokens));
                emit_effect(ctx, AgentEffect::AddReasoningTokens(reasoning_tokens));
                emit_effect(ctx, AgentEffect::SetStopReason(StopReason::ReasoningBudgetExceeded));
                emit_effect(ctx, AgentEffect::SetLastResponse(response.clone()));
                // Emit state bridge for edge conditions
                let bridged = AgentState {
                    iterations: state.iterations + 1,
                    output_tokens: state.output_tokens + output_tokens,
                    reasoning_tokens: state.reasoning_tokens + reasoning_tokens,
                    stop_reason: Some(StopReason::ReasoningBudgetExceeded),
                    last_response: Some(response.clone()),
                    ..state
                };
                emit_state_bridge(ctx, &bridged);
                ctx.end();
                return Ok(());
            }
        }

        // 9. Emit Token Effects (dual-write)
        emit_effect(ctx, AgentEffect::AddOutputTokens(output_tokens));
        emit_effect(ctx, AgentEffect::AddReasoningTokens(reasoning_tokens));

        // 10. 检查总输出预算（用本地累加判断，因为 Effect 还未 apply）
        if state.exceeded_output_with_extra(
            self.config.max_total_output_tokens,
            output_tokens,
        ) {
            emit_effect(ctx, AgentEffect::SetStopReason(StopReason::OutputBudgetExceeded));
            emit_effect(ctx, AgentEffect::SetLastResponse(response.clone()));
            let bridged = AgentState {
                iterations: state.iterations + 1,
                output_tokens: state.output_tokens + output_tokens,
                reasoning_tokens: state.reasoning_tokens + reasoning_tokens,
                stop_reason: Some(StopReason::OutputBudgetExceeded),
                ..state
            };
            emit_state_bridge(ctx, &bridged);
            ctx.end();
            return Ok(());
        }

        // 11. 检查总推理预算
        if state.exceeded_reasoning_with_extra(
            self.config.max_total_reasoning_tokens,
            reasoning_tokens,
        ) {
            emit_effect(ctx, AgentEffect::SetStopReason(StopReason::ReasoningBudgetExceeded));
            emit_effect(ctx, AgentEffect::SetLastResponse(response.clone()));
            let bridged = AgentState {
                iterations: state.iterations + 1,
                output_tokens: state.output_tokens + output_tokens,
                reasoning_tokens: state.reasoning_tokens + reasoning_tokens,
                stop_reason: Some(StopReason::ReasoningBudgetExceeded),
                ..state
            };
            emit_state_bridge(ctx, &bridged);
            ctx.end();
            return Ok(());
        }

        // 12. Emit 消息追加 Effect
        let content = response.content.clone();
        let msg = Message::Assistant { content };
        emit_effect(ctx, AgentEffect::AppendMessage(msg));

        // 13. 检查是否有 tool_calls
        let has_tool_calls = response.has_tool_calls();

        if has_tool_calls {
            emit_effect(ctx, AgentEffect::AddToolCalls(tool_calls_count));
            // Emit state bridge for edge conditions
            let bridged = AgentState {
                iterations: state.iterations + 1,
                output_tokens: state.output_tokens + output_tokens,
                reasoning_tokens: state.reasoning_tokens + reasoning_tokens,
                total_tool_calls: state.total_tool_calls + tool_calls_count,
                ..state
            };
            emit_state_bridge(ctx, &bridged);
            tracing::debug!(
                iteration = state.iterations + 1,
                tool_calls = tool_calls_count,
                "LLM call completed, executing tools"
            );
        } else {
            emit_effect(ctx, AgentEffect::SetStopReason(StopReason::Complete));
            emit_effect(ctx, AgentEffect::SetLastResponse(response.clone()));
            let bridged = AgentState {
                iterations: state.iterations + 1,
                output_tokens: state.output_tokens + output_tokens,
                reasoning_tokens: state.reasoning_tokens + reasoning_tokens,
                stop_reason: Some(StopReason::Complete),
                last_response: Some(response),
                ..state
            };
            emit_state_bridge(ctx, &bridged);
            ctx.end();
            tracing::debug!(
                iteration = state.iterations + 1,
                "LLM call completed, no tool calls"
            );
        }

        Ok(())
    }
}

// ─── ToolNode ─────────────────────────────────────────────────

/// 工具执行节点 — 读取 tool_calls，执行工具，写入 results。
///
/// # Typed State
///
/// 从 ctx 获取 `AgentState`，执行工具，追加结果到消息历史。
pub struct ToolNode {
    pub name: String,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
}

impl Clone for ToolNode {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            executor: self.executor.clone(),
            config: self.config.clone(),
        }
    }
}

impl ToolNode {
    pub fn new(name: impl Into<String>, executor: ToolExecutor, config: ToolUseConfig) -> Self {
        Self {
            name: name.into(),
            executor,
            config,
        }
    }
}

#[async_trait]
impl FlowNode for ToolNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        use super::typed_state::AgentEffect;

        // 1. 获取工具调用
        let round = ResolvedRound::new(self.executor.snapshot().await);
        let state = get_agent_state(ctx);
        let last_response = state.last_response.unwrap_or_else(empty_response);
        let tool_calls: Vec<ToolCall> = last_response.tool_calls().cloned().collect();

        if tool_calls.is_empty() {
            return Ok(());
        }

        // 2. 执行工具
        let batch =
            execute_batch_with(&tool_calls, &round.snapshot, &self.executor.retry_policy()).await;

        if batch.panicked {
            tracing::warn!("tool batch task panicked — error results filled in by executor");
        }

        // 3. 应用预算截断
        let results: Vec<Message> = batch
            .results
            .into_iter()
            .map(|m| {
                if let Message::ToolResult {
                    ref tool_call_id,
                    is_error: false,
                    ref content,
                } = m
                {
                    let truncated = self
                        .config
                        .context_budget
                        .truncate_tool_result_blocks(content);
                    if truncated != *content {
                        return Message::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            is_error: false,
                            content: truncated,
                        };
                    }
                }
                m
            })
            .collect();

        // 4. StreamChunk emit (v04 #1) — 工具执行结果
        for result in &results {
            if let Message::ToolResult {
                tool_call_id,
                content,
                is_error,
            } = result
            {
                let content_str: String = content
                    .iter()
                    .filter_map(|b| match b {
                        lellm_core::ContentBlock::Text(t) => Some(t.text.clone()),
                        lellm_core::ContentBlock::Image { .. }
                        | lellm_core::ContentBlock::Thinking(_)
                        | lellm_core::ContentBlock::ToolCall(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                ctx.emit(lellm_graph::StreamChunk::ToolResult {
                    id: tool_call_id.clone(),
                    content: content_str,
                    is_error: *is_error,
                });
            }
        }

        // 5. Emit 消息追加 Effect（不直接改 state）
        ctx.emit_effect(AgentEffect::AppendMessages(results));

        tracing::debug!(tool_calls = tool_calls.len(), "tool execution completed");

        Ok(())
    }
}

// ─── ReactCondition ───────────────────────────────────────────

/// ReAct 循环条件 — 检查 tool_calls 是否为空。
///
/// 有 tool_calls → Goto("tool")
/// 无 tool_calls → End
pub struct ReactCondition {
    pub name: String,
}

impl ReactCondition {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl FlowNode for ReactCondition {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let state = get_agent_state(ctx);
        // 从最后一条 Assistant 消息判断是否有 tool_calls
        let has_tool_calls = state.messages.iter().rev().find_map(|m| {
            if let Message::Assistant { content } = m {
                Some(content.iter().any(|b| matches!(b, ContentBlock::ToolCall(_))))
            } else {
                None
            }
        }).unwrap_or(false);

        if has_tool_calls {
            ctx.goto("tool");
        } else {
            ctx.end();
        }

        Ok(())
    }
}

// ─── CompactorNode ────────────────────────────────────────────

/// 上下文压缩节点 — 独立 FlowNode，职责单一。
///
/// # Typed State
///
/// 从 AgentState 获取消息历史，压缩后替换。
pub struct CompactorNode {
    pub name: String,
    pub compactor: Box<dyn ContextCompactor>,
    pub budget: ContextBudget,
}

impl Clone for CompactorNode {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            compactor: Box::new(LocalCompactor::new()),
            budget: self.budget.clone(),
        }
    }
}

impl CompactorNode {
    pub fn new(
        name: impl Into<String>,
        compactor: Box<dyn ContextCompactor>,
        budget: ContextBudget,
    ) -> Self {
        Self {
            name: name.into(),
            compactor,
            budget,
        }
    }
}

#[async_trait]
impl FlowNode for CompactorNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        use super::typed_state::AgentEffect;

        let state = get_agent_state(ctx);

        if !self.budget.should_compact(state.output_tokens) {
            return Ok(());
        }

        let result = self.compactor.compact(&state.messages, &self.budget);

        // 只有实际压缩了才 emit Effects
        if result.removed_messages > 0 {
            ctx.emit_effect(AgentEffect::ReplaceMessages(result.messages));
            ctx.emit_effect(AgentEffect::IncrementCompactCount);

            tracing::debug!(
                agent = %self.name,
                before_tokens = result.before_tokens,
                after_tokens = result.after_tokens,
                removed = result.removed_messages,
                "context compacted"
            );
        }

        Ok(())
    }
}

// ─── BudgetCondition ──────────────────────────────────────────

/// 预算条件节点 — 检查 Token 预算，决定是否进入 Compactor。
///
/// 预算充足 → Goto("llm")
/// 需要压缩 → Goto("compactor")
pub struct BudgetCondition {
    pub name: String,
    pub budget: ContextBudget,
}

impl BudgetCondition {
    pub fn new(name: impl Into<String>, budget: ContextBudget) -> Self {
        Self {
            name: name.into(),
            budget,
        }
    }
}

#[async_trait]
impl FlowNode for BudgetCondition {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let state = get_agent_state(ctx);

        if self.budget.should_compact(state.output_tokens) {
            ctx.goto("compactor");
        } else {
            ctx.goto("llm");
        }

        Ok(())
    }
}

// ─── build_react_graph ────────────────────────────────────────

/// 构建 ReAct 内部图。
///
/// ```text
/// START → budget_check
///
/// budget_check --budget_ok--> [llm]
///          --need_compact--> [compactor] → [llm]
///
/// [llm] → [tool_decision]
///    --has_tool_calls--> [tool] → [budget_check] (循环)
///    --no_tool_calls--> [end]
/// ```
pub fn build_react_graph(
    llm_node: LLMNode,
    tool_node: ToolNode,
    compactor_node: CompactorNode,
) -> Graph {
    let llm_name = llm_node.name.clone();
    let budget = llm_node.config.context_budget.clone();

    let mut builder = GraphBuilder::new(format!("react_{}", llm_name));
    builder.start("budget_check");
    builder.end("end");

    // 节点注册
    builder.node("llm", NodeKind::External(Arc::new(llm_node)));
    builder.node("tool", NodeKind::External(Arc::new(tool_node)));
    builder.node(
        "tool_decision",
        NodeKind::External(Arc::new(ReactCondition::new(format!(
            "{}_tool_decision",
            llm_name
        )))),
    );
    builder.node(
        "budget_check",
        NodeKind::External(Arc::new(BudgetCondition::new(
            format!("{}_budget", llm_name),
            budget.clone(),
        ))),
    );
    builder.node("compactor", NodeKind::External(Arc::new(compactor_node)));
    // End 节点 — no-op 终端节点（LLMNode 通过 ctx.end() 终止，实际不会执行到此）
    builder.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));

    // budget_check → llm (预算充足，直接走 LLM)
    let budget_clone = budget.clone();
    builder.edge_if("budget_check", "llm", move |state| {
        let tokens: usize = state
            .get(SK_OUTPUT_TOKENS)
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0);
        !budget_clone.should_compact(tokens)
    });

    // budget_check → compactor (需要压缩)
    builder.edge_fallback("budget_check", "compactor");

    // compactor → llm (压缩完直接到 LLM)
    builder.edge("compactor", "llm");

    // llm → tool_decision
    builder.edge("llm", "tool_decision");

    // tool_decision → tool (有 tool_calls)
    builder.edge_if("tool_decision", "tool", |state| {
        state
            .get(SK_HAS_TOOL_CALLS)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });

    // tool_decision → end (无 tool_calls)
    builder.edge_fallback("tool_decision", "end");

    // tool → budget_check (工具执行完，回到预算检查，形成循环)
    builder.edge("tool", "budget_check");

    builder.build().expect("ReAct graph should be valid")
}
