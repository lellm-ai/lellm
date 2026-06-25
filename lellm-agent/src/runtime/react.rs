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
use super::context::{ContextBudget, ContextCompactor, estimate_reasoning_block, estimate_text};
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

/// 估算单轮响应中的推理 Token 数。
fn estimate_round_reasoning_tokens(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Thinking(th) => Some(estimate_reasoning_block(th)),
            _ => None,
        })
        .sum()
}

// ─── LLMNode ──────────────────────────────────────────────────

/// LLM 调用节点 — 执行单次 LLM 调用。
///
/// **职责单一：** 只负责"调用 LLM + 收集流式响应 + emit Effects"。
/// 不感知 Budget、Compaction、Iteration Limit 等运行时策略。
///
/// # Typed State
///
/// 从 ctx 获取 `AgentState`，直接操作 typed 字段，写回 ctx。
#[derive(Clone)]
pub struct LLMNode {
    pub name: String,
    pub model: ResolvedModel,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
    pub deps: ToolUseDeps,
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
        // 1. 获取 AgentState
        let state = ctx.state().clone();

        // 2. Emit 迭代递增 Effect
        ctx.emit_effect(AgentEffect::IncrementIteration);

        // 3. 获取工具定义 & 构建 LLM 请求
        let round = ResolvedRound::new(self.executor.snapshot().await);

        let req = build_request_inner_with_round(
            &self.model,
            &state.messages,
            self.config.max_output_tokens,
            &self.config.request_options,
            state.iterations + 1,
            &round.definitions,
        );

        // 4. 执行 LLM 流式调用
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
                    ctx.emit(lellm_graph::StreamChunk::TextDelta(token.clone()));
                    current_text.push_str(&token);
                }
                Ok(ProviderEvent::ThinkingDelta { thinking, .. }) => {
                    ctx.emit(lellm_graph::StreamChunk::ThinkingDelta(thinking.clone()));
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

        // 5. 分离 output / reasoning token，Emit Token Effects
        let (output_tokens, reasoning_tokens) = split_output_tokens(&response.content);
        ctx.emit_effect(AgentEffect::AddOutputTokens(output_tokens));
        ctx.emit_effect(AgentEffect::AddReasoningTokens(reasoning_tokens));

        // 6. Emit 消息追加 Effect
        let content = response.content.clone();
        let msg = Message::Assistant { content };
        ctx.emit_effect(AgentEffect::AppendMessage(msg));

        // 7. 记录 tool_calls
        let has_tools = response.has_tool_calls();
        if has_tools {
            ctx.emit_effect(AgentEffect::AddToolCalls(tool_calls_count));
        }

        // 8. Emit LastResponse（供 PostLLMGuard 检查）
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
#[derive(Clone)]
pub struct ToolNode {
    pub name: String,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
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
                ctx.emit(lellm_graph::StreamChunk::ToolOutput {
                    call_id: tool_call_id.clone(),
                    tool_name: "".to_string(),
                    content: content_str,
                    is_error: *is_error,
                    duration: std::time::Duration::ZERO,
                });
            }
        }

        // 5. Emit 消息追加 Effect（不直接改 state）
        // context_tokens 由 estimated_context_tokens() 实时派生，无需手动累加
        ctx.emit_effect(AgentEffect::AppendMessages(results));

        tracing::debug!(tool_calls = tool_calls.len(), "tool execution completed");

        Ok(())
    }
}

// ─── StopConfig ───────────────────────────────────────────────

/// 终止条件配置 — 从 LLMNode 提取的运行时策略。
///
/// 由 PostLLMGuard 持有，LLMNode 完全不知道这些概念。
#[derive(Debug, Clone)]
pub struct StopConfig {
    /// 最大迭代轮次
    pub max_iterations: usize,
    /// 单轮推理 Token 上限
    pub max_reasoning_tokens: Option<u32>,
    /// 总输出 Token 上限
    pub max_total_output_tokens: Option<u32>,
    /// 总推理 Token 上限
    pub max_total_reasoning_tokens: Option<u32>,
}

impl StopConfig {
    pub fn from_tool_use_config(config: &ToolUseConfig) -> Self {
        Self {
            max_iterations: config.max_iterations,
            max_reasoning_tokens: config.request_options.max_reasoning_tokens,
            max_total_output_tokens: config.max_total_output_tokens,
            max_total_reasoning_tokens: config.max_total_reasoning_tokens,
        }
    }
}

// ─── PostLLMGuard ─────────────────────────────────────────────

/// LLM 调用后的后置检查节点 — 统一处理所有终止条件与路由。
///
/// 从 `LLMNode` 提取的全部运行时策略：
/// - 最大迭代检查
/// - Budget 检查（单轮推理 / 总输出 / 总推理）
/// - 路由决策（Tool / End）
///
/// 检查顺序：
/// 1. 已终止（stop_reason 已设置）→ End
/// 2. 超过最大迭代 → End
/// 3. 单轮推理超限 → End
/// 4. 总输出超限 → End
/// 5. 总推理超限 → End
/// 6. 有 tool_calls → Goto("tool")
/// 7. 无 tool_calls → End（正常完成）
pub struct PostLLMGuard {
    pub name: String,
    pub stop_config: StopConfig,
}

impl PostLLMGuard {
    pub fn new(name: impl Into<String>, stop_config: StopConfig) -> Self {
        Self {
            name: name.into(),
            stop_config,
        }
    }
}

#[async_trait]
impl FlowNode<AgentState> for PostLLMGuard {
    async fn execute(&self, ctx: &mut NodeContext<'_, AgentState>) -> Result<(), GraphError> {
        let state = ctx.state().clone();

        // 1. 已终止（前置节点已设置 stop_reason）→ End
        if state.stop_reason.is_some() {
            ctx.end();
            return Ok(());
        }

        // 2. 超过最大迭代 → End
        if state.reached_max(self.stop_config.max_iterations) {
            ctx.emit_effect(AgentEffect::SetStopReason(StopReason::MaxIterationsReached));
            ctx.end();
            return Ok(());
        }

        // 3-5. Budget 检查（优先级：单轮推理 > 总输出 > 总推理）
        let last_response = state.last_response.clone().unwrap_or_else(empty_response);
        let mut stopped = false;

        // 3. 单轮推理 Token 超限
        if let Some(limit) = self.stop_config.max_reasoning_tokens {
            let round_reasoning = estimate_round_reasoning_tokens(&last_response.content);
            if round_reasoning > limit as usize {
                tracing::warn!(
                    round_reasoning,
                    max_reasoning_tokens = limit,
                    "single-round reasoning budget exceeded"
                );
                ctx.emit_effect(AgentEffect::SetStopReason(
                    StopReason::ReasoningBudgetExceeded,
                ));
                stopped = true;
            }
        }

        // 4. 总输出 Token 超限
        if !stopped && state.exceeded_output(self.stop_config.max_total_output_tokens) {
            ctx.emit_effect(AgentEffect::SetStopReason(StopReason::OutputBudgetExceeded));
            stopped = true;
        }

        // 5. 总推理 Token 超限
        if !stopped && state.exceeded_reasoning(self.stop_config.max_total_reasoning_tokens) {
            ctx.emit_effect(AgentEffect::SetStopReason(
                StopReason::ReasoningBudgetExceeded,
            ));
        }

        if stopped {
            ctx.end();
            return Ok(());
        }

        // 6. 有 tool_calls → 去执行工具
        if last_response.has_tool_calls() {
            ctx.goto("tool");
            return Ok(());
        }

        // 7. 无 tool_calls → 正常完成
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

        if !self.budget.should_compact(state.estimated_context_tokens()) {
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

        if self.budget.should_compact(state.estimated_context_tokens()) {
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
    let stop_config = StopConfig::from_tool_use_config(&llm_node.config);

    let mut builder =
        GraphBuilder::<AgentState, AgentStateMerge>::new(format!("react_{}", llm_name));
    builder.start("budget_check");
    builder.end("end");

    // 节点注册
    builder.node("llm", NodeKind::External(Arc::new(llm_node)));
    builder.node("tool", NodeKind::External(Arc::new(tool_node)));
    builder.node(
        "post_llm_check",
        NodeKind::External(Arc::new(PostLLMGuard::new(
            format!("{}_post_llm", llm_name),
            stop_config,
        ))),
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
    // 条件节点通过 ctx.goto()/ctx.end() 控制路由，NextAction::Goto 优先于 edge 解析。
    //
    // 静态边与运行时路由的对应关系：
    //   budget_check → llm          (BudgetCondition: 预算充足时 goto("llm"))
    //   budget_check → compactor    (BudgetCondition: 需要压缩时 goto("compactor"))
    //   compactor → llm             (CompactorNode: 压缩后走下一步，无显式 goto)
    //   llm → post_llm_check        (LLMNode: 调用完走下一步，无显式 goto)
    //   post_llm_check → tool       (PostLLMGuard: 有 tool_calls 时 goto("tool"))
    //   post_llm_check → end        (PostLLMGuard: 无 tool_calls 或 budget 超限时 end())
    //   tool → budget_check         (ToolNode: 执行完走下一步，无显式 goto)
    builder.edge("budget_check", "llm");
    builder.edge_fallback("budget_check", "compactor");
    builder.edge("compactor", "llm");
    builder.edge("llm", "post_llm_check");
    builder.edge("post_llm_check", "tool");
    builder.edge_fallback("post_llm_check", "end");
    builder.edge("tool", "budget_check");

    builder.build().expect("ReAct graph should be valid")
}
