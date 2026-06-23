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
    FlowNode, Graph, GraphBuilder, GraphError, NodeContext, NodeKind, TaskNode, TerminalError,
};
use lellm_provider::ProviderEvent;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_inner_with_round, empty_response};
use super::context::{
    AgentExecutionContext, ContextBudget, ContextCompactor, estimate_reasoning_block, estimate_text,
};
use super::event::StopReason;
use super::runtime::ResolvedRound;
use super::tools::{ToolExecutor, execute_batch_with};
use super::typed_state::{AgentEffect, AgentState, AgentStateMerge};
use lellm_provider::ResolvedModel;

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
impl FlowNode<AgentState> for LLMNode {
    async fn execute(&self, ctx: &mut NodeContext<'_, AgentState>) -> Result<(), GraphError> {
        // 1. 获取 AgentState（直接读取，零序列化）
        let state = ctx.state().clone();

        // 2. 检查最大迭代 — 超限则 emit stop_reason，由 PostLLMGuard 路由到 End
        if state.reached_max(self.config.max_iterations) {
            ctx.emit_effect(AgentEffect::SetStopReason(StopReason::MaxIterationsReached));
            let last_response = state.last_response.clone().unwrap_or_else(empty_response);
            ctx.emit_effect(AgentEffect::SetLastResponse(last_response));
            return Ok(());
        }

        let mut exec_ctx = AgentExecutionContext::new(state.messages_ref());

        // 3. Emit 迭代递增 Effect
        ctx.emit_effect(AgentEffect::IncrementIteration);

        // 3. 获取工具定义
        let round = ResolvedRound::new(self.executor.snapshot().await);

        // 4. 构建 LLM 请求（使用新迭代数）
        let req = build_request_inner_with_round(
            &self.model,
            &state.messages,
            self.config.max_output_tokens,
            &self.config.request_options,
            state.iterations + 1,
            &round.definitions,
        );

        // 5. 执行 LLM 流式调用（v04: 真正的流式输出）
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

        // 6. 分离 output / reasoning token
        let (output_tokens, reasoning_tokens) = split_output_tokens(&response.content);
        exec_ctx.add_tokens(output_tokens + reasoning_tokens);

        // 7. Emit Token Effects
        ctx.emit_effect(AgentEffect::AddOutputTokens(output_tokens));
        ctx.emit_effect(AgentEffect::AddReasoningTokens(reasoning_tokens));

        // 8. 检查 reasoning budget（单轮）→ emit stop_reason，路由交给 PostLLMGuard
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
                ctx.emit_effect(AgentEffect::SetStopReason(
                    StopReason::ReasoningBudgetExceeded,
                ));
            }
        }

        // 9. 检查总输出预算 → emit stop_reason，路由交给 PostLLMGuard
        if state.exceeded_output_with_extra(self.config.max_total_output_tokens, output_tokens)
            && state.stop_reason.is_none()
        {
            ctx.emit_effect(AgentEffect::SetStopReason(StopReason::OutputBudgetExceeded));
        }

        // 10. 检查总推理预算 → emit stop_reason，路由交给 PostLLMGuard
        if state
            .exceeded_reasoning_with_extra(self.config.max_total_reasoning_tokens, reasoning_tokens)
            && state.stop_reason.is_none()
        {
            ctx.emit_effect(AgentEffect::SetStopReason(
                StopReason::ReasoningBudgetExceeded,
            ));
        }

        // 11. Emit 消息追加 Effect
        let content = response.content.clone();
        let msg = Message::Assistant { content };
        ctx.emit_effect(AgentEffect::AppendMessage(msg));

        // 12. 记录是否有 tool_calls（在 move response 之前）
        let has_tools = response.has_tool_calls();
        if has_tools {
            ctx.emit_effect(AgentEffect::AddToolCalls(tool_calls_count));
        }

        // 13. Emit LastResponse（供 PostLLMGuard 检查）
        ctx.emit_effect(AgentEffect::SetLastResponse(response));

        // 路由决策全部交给 PostLLMGuard，此处不调用 ctx.goto()/ctx.end()
        tracing::debug!(
            iteration = state.iterations + 1,
            has_tool_calls = has_tools,
            "LLM call completed"
        );

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
impl FlowNode<AgentState> for ToolNode {
    async fn execute(&self, ctx: &mut NodeContext<'_, AgentState>) -> Result<(), GraphError> {
        // 1. 获取工具调用
        let round = ResolvedRound::new(self.executor.snapshot().await);
        let state = ctx.state().clone();
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

// ─── PostLLMGuard ─────────────────────────────────────────────

/// LLM 调用后的后置检查节点 — 统一处理所有终止条件与路由。
///
/// 从 `LLMNode` 提取出来，确保 LLMNode 职责单一（只负责调用）。
///
/// 检查顺序：
/// 1. 已终止（stop_reason 已设置）→ End
/// 2. 有 tool_calls → Goto("tool")
/// 3. 无 tool_calls → End（正常完成）
pub struct PostLLMGuard {
    pub name: String,
}

impl PostLLMGuard {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl FlowNode<AgentState> for PostLLMGuard {
    async fn execute(&self, ctx: &mut NodeContext<'_, AgentState>) -> Result<(), GraphError> {
        let state = ctx.state();

        // 1. LLMNode 已设置 stop_reason（budget 超限等）→ 终止
        if state.stop_reason.is_some() {
            ctx.end();
            return Ok(());
        }

        // 2. 有 tool_calls → 去执行工具
        if state
            .last_response
            .as_ref()
            .is_some_and(|r| r.has_tool_calls())
        {
            ctx.goto("tool");
            return Ok(());
        }

        // 3. 无 tool_calls → 正常完成
        ctx.emit_effect(AgentEffect::SetStopReason(StopReason::Complete));
        ctx.end();

        Ok(())
    }
}

// ─── CompactorNode ────────────────────────────────────────────

/// 上下文压缩节点 — 独立 FlowNode，职责单一。
///
/// # Typed State
///
/// 从 AgentState 获取消息历史，压缩后替换。
#[derive(Clone)]
pub struct CompactorNode {
    pub name: String,
    pub compactor: Arc<dyn ContextCompactor>,
    pub budget: ContextBudget,
}

impl CompactorNode {
    pub fn new(
        name: impl Into<String>,
        compactor: Arc<dyn ContextCompactor>,
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
impl FlowNode<AgentState> for CompactorNode {
    async fn execute(&self, ctx: &mut NodeContext<'_, AgentState>) -> Result<(), GraphError> {
        let state = ctx.state();

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
impl FlowNode<AgentState> for BudgetCondition {
    async fn execute(&self, ctx: &mut NodeContext<'_, AgentState>) -> Result<(), GraphError> {
        let state = ctx.state();

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
/// [llm] → [post_llm_check]
///    --budget_exceeded--> [end]
///    --has_tool_calls--> [tool] → [budget_check] (循环)
///    --no_tool_calls--> [end]
/// ```
///
/// 使用 `Graph<AgentState>` — 节点直接读写强类型 AgentState，零序列化。
pub fn build_react_graph(
    llm_node: LLMNode,
    tool_node: ToolNode,
    compactor_node: CompactorNode,
) -> Graph<AgentState, AgentStateMerge> {
    let llm_name = llm_node.name.clone();
    let budget = llm_node.config.context_budget.clone();

    let mut builder =
        GraphBuilder::<AgentState, AgentStateMerge>::new(format!("react_{}", llm_name));
    builder.start("budget_check");
    builder.end("end");

    // 节点注册
    builder.node("llm", NodeKind::External(Arc::new(llm_node)));
    builder.node("tool", NodeKind::External(Arc::new(tool_node)));
    builder.node(
        "post_llm_check",
        NodeKind::External(Arc::new(PostLLMGuard::new(format!(
            "{}_post_llm",
            llm_name
        )))),
    );
    builder.node(
        "budget_check",
        NodeKind::External(Arc::new(BudgetCondition::new(
            format!("{}_budget", llm_name),
            budget,
        ))),
    );
    builder.node("compactor", NodeKind::External(Arc::new(compactor_node)));
    // End 节点 — no-op 终端节点
    builder.node(
        "end",
        NodeKind::Task(TaskNode::<AgentState>::new("end", |_| Ok(()))),
    );

    // 注意：以下 edges 仅用于静态分析（analyze/diagnostics），运行时不使用。
    // BudgetCondition、PostLLMGuard 通过 ctx.goto()/ctx.end() 控制路由，
    // executor 的 NextAction::Goto 优先于 edge 解析。
    builder.edge("budget_check", "llm");
    builder.edge_fallback("budget_check", "compactor");
    builder.edge("compactor", "llm");
    builder.edge("llm", "post_llm_check");
    builder.edge("post_llm_check", "tool");
    builder.edge_fallback("post_llm_check", "end");
    builder.edge("tool", "budget_check");

    builder.build().expect("ReAct graph should be valid")
}
