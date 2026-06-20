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

use lellm_core::{ChatResponse, Message, ToolCall};
use lellm_graph::{
    FlowNode, Graph, GraphBuilder, GraphError, NodeContext, NodeKind, TerminalError,
};

use super::config::{ToolUseConfig, ToolUseDeps, build_request_inner_with_round, empty_response};
use super::context::{
    AgentExecutionContext, ContextCompactor, ContextBudget, LocalCompactor,
    estimate_reasoning_block, estimate_text,
};
use super::event::StopReason;
use super::iteration::execute_with_fallback;
use super::runtime::ResolvedRound;
use super::tools::{ToolExecutor, execute_batch_with};
use super::typed_state::{AgentState, AGENT_STATE_KEY};
use lellm_provider::ResolvedModel;

// ─── State Keys（边条件等仍需要字符串 key）──────────────────

pub const SK_MESSAGES: &str = "messages";
pub const SK_ITERATIONS: &str = "iterations";
pub const SK_TOTAL_TOOL_CALLS: &str = "total_tool_calls";
pub const SK_OUTPUT_TOKENS: &str = "output_tokens";
pub const SK_REASONING_TOKENS: &str = "reasoning_tokens";
pub const SK_HAS_TOOL_CALLS: &str = "has_tool_calls";
pub const SK_STOP_REASON: &str = "stop_reason";
pub const SK_LAST_RESPONSE: &str = "last_response";
pub const SK_COMPACT_COUNT: &str = "compact_count";

// ─── Typed State 辅助函数 ──────────────────────────────────────

/// 从 NodeContext 获取 AgentState。不存在则创建空状态。
fn get_agent_state(ctx: &NodeContext<'_>) -> AgentState {
    ctx.get_state::<AgentState>(AGENT_STATE_KEY)
        .unwrap_or_default()
}

/// 将 AgentState 写回 NodeContext。
fn set_agent_state(ctx: &mut NodeContext<'_>, state: &AgentState) {
    ctx.set_state(AGENT_STATE_KEY, state.clone());
}

/// 同步 AgentState 的关键字段到 ctx（供边条件使用）。
///
/// 边条件（`edge_if`）直接读取 `State`（HashMap），不感知 AgentState。
/// 此函数桥接 typed state → dynamic state。
fn sync_to_ctx(ctx: &mut NodeContext<'_>, state: &AgentState) {
    ctx.set(SK_ITERATIONS, state.iterations as u64);
    ctx.set(SK_TOTAL_TOOL_CALLS, state.total_tool_calls as u64);
    ctx.set(SK_OUTPUT_TOKENS, state.output_tokens as u64);
    ctx.set(SK_REASONING_TOKENS, state.reasoning_tokens as u64);
    ctx.set(SK_COMPACT_COUNT, state.compact_count as u64);

    // messages 以 JSON 数组存储（边条件可能读取）
    let messages_json: Vec<serde_json::Value> = state
        .messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    ctx.set(SK_MESSAGES, serde_json::json!(messages_json));
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
        // 1. 获取 AgentState
        let mut state = get_agent_state(ctx);
        let mut exec_ctx = AgentExecutionContext::new(state.messages_ref());

        // 2. 检查最大迭代
        if state.reached_max(self.config.max_iterations) {
            let last_response = ctx
                .get::<Option<ChatResponse>>(SK_LAST_RESPONSE)
                .flatten()
                .unwrap_or_else(empty_response);
            ctx.set(
                SK_STOP_REASON,
                format!("{:?}", StopReason::MaxIterationsReached),
            );
            ctx.set(
                SK_LAST_RESPONSE,
                serde_json::to_value(&last_response).unwrap_or_default(),
            );
            ctx.end();
            return Ok(());
        }

        // 3. 递增迭代
        state.iterations += 1;

        // 4. 获取工具定义
        let round = ResolvedRound::new(self.executor.snapshot().await);

        // 5. 构建 LLM 请求
        let req = build_request_inner_with_round(
            &self.model,
            &state.messages,
            self.config.max_output_tokens,
            &self.config.request_options,
            state.iterations,
            &round.definitions,
        );

        // 6. 执行 LLM 调用（带 fallback）
        let response = execute_with_fallback(
            &self.deps.fallback,
            |_| true,
            || self.model.provider.call(&req),
            state.iterations,
            &state.messages,
        )
        .await
        .map_err(|e| {
            GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: self.name.clone(),
                source: e.into(),
            })
        })?;

        // 7. 检查 reasoning budget（单轮）
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

                let (ot, rt) = split_output_tokens(&response.content);
                state.output_tokens += ot;
                state.reasoning_tokens += rt;
                exec_ctx.add_tokens(ot + rt);
                set_agent_state(ctx, &state);
                ctx.set(
                    SK_STOP_REASON,
                    format!("{:?}", StopReason::ReasoningBudgetExceeded),
                );
                ctx.set(
                    SK_LAST_RESPONSE,
                    serde_json::to_value(&response).unwrap_or_default(),
                );
                ctx.end();
                return Ok(());
            }
        }

        // 8. 记录输出 token
        let (output_tokens, reasoning_tokens) = split_output_tokens(&response.content);
        state.output_tokens += output_tokens;
        state.reasoning_tokens += reasoning_tokens;
        exec_ctx.add_tokens(output_tokens + reasoning_tokens);

        // 9. 检查总输出预算
        if state.exceeded_output(self.config.max_total_output_tokens) {
            set_agent_state(ctx, &state);
            ctx.set(
                SK_STOP_REASON,
                format!("{:?}", StopReason::OutputBudgetExceeded),
            );
            ctx.set(
                SK_LAST_RESPONSE,
                serde_json::to_value(&response).unwrap_or_default(),
            );
            ctx.end();
            return Ok(());
        }

        // 10. 检查总推理预算
        if state.exceeded_reasoning(self.config.max_total_reasoning_tokens) {
            set_agent_state(ctx, &state);
            ctx.set(
                SK_STOP_REASON,
                format!("{:?}", StopReason::ReasoningBudgetExceeded),
            );
            ctx.set(
                SK_LAST_RESPONSE,
                serde_json::to_value(&response).unwrap_or_default(),
            );
            ctx.end();
            return Ok(());
        }

        // 11. 写入 assistant 响应到消息历史
        let content = response.content.clone();
        let msg = Message::Assistant { content };
        state.messages.push(msg);

        // 12. 检查是否有 tool_calls
        let has_tool_calls = response.has_tool_calls();
        let tool_calls: Vec<ToolCall> = response.tool_calls().cloned().collect();

        if has_tool_calls {
            state.total_tool_calls += tool_calls.len();
        }

        // 13. 同步状态到 ctx
        set_agent_state(ctx, &state);
        sync_to_ctx(ctx, &state);
        ctx.set(SK_HAS_TOOL_CALLS, has_tool_calls);

        // 13.5. StreamChunk emit (v04 #1) — 阻塞模式一次性发射完整 response
        for block in &response.content {
            match block {
                lellm_core::ContentBlock::Text(t) => {
                    ctx.emit(lellm_graph::StreamChunk::Text(t.text.clone()));
                }
                lellm_core::ContentBlock::Thinking(th) => {
                    ctx.emit(lellm_graph::StreamChunk::Thinking(th.thinking.clone()));
                }
                _ => {}
            }
        }
        if has_tool_calls {
            for tc in &tool_calls {
                let args_str = match serde_json::to_string(&tc.arguments) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            tool_call_id = %tc.id,
                            tool_name = %tc.name,
                            error = %e,
                            "failed to serialize tool call arguments for StreamChunk"
                        );
                        format!("{{\"error\":\"serialization_failed\",\"detail\":\"{}\"}}", e)
                    }
                };
                ctx.emit(lellm_graph::StreamChunk::ToolCall {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: args_str,
                });
            }
        }

        if has_tool_calls {
            tracing::debug!(
                iteration = state.iterations,
                tool_calls = tool_calls.len(),
                "LLM call completed, executing tools"
            );
        } else {
            ctx.set(SK_STOP_REASON, format!("{:?}", StopReason::Complete));
            ctx.set(
                SK_LAST_RESPONSE,
                serde_json::to_value(&response).unwrap_or_default(),
            );
            ctx.end();
            tracing::debug!(
                iteration = state.iterations,
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
        let mut state = get_agent_state(ctx);
        // 1. 获取工具调用
        let round = ResolvedRound::new(self.executor.snapshot().await);
        let last_response: ChatResponse = ctx
            .get::<Option<ChatResponse>>(SK_LAST_RESPONSE)
            .flatten()
            .unwrap_or_else(empty_response);
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
                    let truncated = self.config.context_budget.truncate_tool_result_blocks(content);
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

        // 4. StreamChunk emit (v04 #1) — 工具执行结果（先 emit，再 extend）
        for result in &results {
            if let Message::ToolResult {
                tool_call_id,
                content,
                is_error,
            } = result
            {
                let mut dropped_types = Vec::new();
                let content_str: String = content
                    .iter()
                    .filter_map(|b| match b {
                        lellm_core::ContentBlock::Text(t) => Some(t.text.clone()),
                        lellm_core::ContentBlock::Image { .. } => {
                            dropped_types.push("Image");
                            None
                        }
                        lellm_core::ContentBlock::Thinking(_) => {
                            dropped_types.push("Thinking");
                            None
                        }
                        lellm_core::ContentBlock::ToolCall(_) => {
                            dropped_types.push("ToolCall");
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !dropped_types.is_empty() {
                    tracing::debug!(
                        tool_call_id = %tool_call_id,
                        dropped_types = ?dropped_types,
                        "ToolResult StreamChunk only emits Text blocks; non-Text blocks dropped"
                    );
                }
                ctx.emit(lellm_graph::StreamChunk::ToolResult {
                    id: tool_call_id.clone(),
                    content: content_str,
                    is_error: *is_error,
                });
            }
        }

        // 5. 追加到消息历史
        state.messages.extend(results);

        // 6. 同步状态到 ctx
        set_agent_state(ctx, &state);
        sync_to_ctx(ctx, &state);

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
        let has_tool_calls: bool = ctx.get(SK_HAS_TOOL_CALLS).unwrap_or(false);

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
        let mut state = get_agent_state(ctx);

        if !self.budget.should_compact(state.output_tokens) {
            return Ok(());
        }

        let result = self.compactor.compact(&state.messages, &self.budget);

        // 只有实际压缩了才更新 state
        if result.removed_messages > 0 {
            state.messages = result.messages;
            state.compact_count += 1;
            set_agent_state(ctx, &state);
            sync_to_ctx(ctx, &state);

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
