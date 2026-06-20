//! ReAct Graph — ToolUseLoop 内部构建的有环图。
//!
//! v04 设计：ToolUseLoop 内部不再手写 while 循环，
//! 构建内部 Graph（LLM Node → Condition → Tool Node → 自环），
//! 调用 `Graph::run_inline()` 驱动循环。
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
    AgentExecutionContext, ContextCompactor, LocalCompactor, estimate_reasoning_block,
};
use super::event::StopReason;
use super::iteration::execute_with_fallback;
use super::runtime::{
    ResolvedRound, get_iterations, get_messages, state_add_output_from_content,
    state_add_tool_calls, state_compact, state_exceeded_total_output,
    state_exceeded_total_reasoning, state_next_iteration, state_push_assistant,
    state_push_tool_results, state_reached_max,
};
use super::tools::{ToolExecutor, execute_batch_with};
use lellm_provider::ResolvedModel;

// ─── State Keys ──────────────────────────────────────────────

pub const SK_MESSAGES: &str = "messages";
pub const SK_ITERATIONS: &str = "iterations";
pub const SK_TOTAL_TOOL_CALLS: &str = "total_tool_calls";
pub const SK_OUTPUT_TOKENS: &str = "output_tokens";
pub const SK_REASONING_TOKENS: &str = "reasoning_tokens";
pub const SK_HAS_TOOL_CALLS: &str = "has_tool_calls";
pub const SK_STOP_REASON: &str = "stop_reason";
pub const SK_LAST_RESPONSE: &str = "last_response";

// ─── LLMNode ──────────────────────────────────────────────────

/// LLM 调用节点 — 执行单次 LLM 调用，包含完整 ToolUseLoop 逻辑。
///
/// 读取 State:
/// - `messages` — 消息历史
/// - `iterations` — 当前迭代轮次
/// - `output_tokens` — 累计输出 Token
/// - `reasoning_tokens` — 累计推理 Token
///
/// 写入 State:
/// - `messages` — 追加 assistant 响应
/// - `iterations` — 递增
/// - `output_tokens` — 累计输出 Token
/// - `reasoning_tokens` — 累计推理 Token
/// - `has_tool_calls` — 是否有工具调用
/// - `stop_reason` — 停止原因（如果达到预算）
pub struct LLMNode {
    pub name: String,
    pub model: ResolvedModel,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
    pub deps: ToolUseDeps,
    pub compactor: Box<dyn ContextCompactor>,
}

impl Clone for LLMNode {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            model: self.model.clone(),
            executor: self.executor.clone(),
            config: self.config.clone(),
            deps: self.deps.clone(),
            compactor: Box::new(LocalCompactor::new()),
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
            compactor: Box::new(LocalCompactor::new()),
        }
    }
}

#[async_trait]
impl FlowNode for LLMNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let messages = get_messages_from_ctx(ctx);
        let mut state = create_state_from_ctx(ctx);
        let mut exec_ctx = AgentExecutionContext::new(&messages);

        // 1. 检查最大迭代
        if state_reached_max(&state, self.config.max_iterations) {
            let last_response: ChatResponse = ctx
                .get_raw(SK_LAST_RESPONSE)
                .and_then(|v| serde_json::from_value(v.clone()).ok())
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

        // 2. 递增迭代
        state_next_iteration(&mut state);

        // 3. Context compaction
        if let Some(_compact_result) = state_compact(
            &mut state,
            &mut exec_ctx,
            &self.config.context_budget,
            &*self.compactor,
        ) {
            tracing::debug!("context compacted");
        }

        // 4. 获取工具定义
        let round = ResolvedRound::new(self.executor.snapshot().await);

        // 5. 构建 LLM 请求
        let req = build_request_inner_with_round(
            &self.model,
            &get_messages(&state),
            self.config.max_output_tokens,
            &self.config.request_options,
            get_iterations(&state) as usize,
            &round.definitions,
        );

        // 6. 执行 LLM 调用（带 fallback）
        let iteration = get_iterations(&state) as usize;
        let msg_snapshot = get_messages(&state);
        let response = execute_with_fallback(
            &self.deps.fallback,
            |_| true,
            || self.model.provider.call(&req),
            iteration,
            &msg_snapshot,
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
                state_add_output_from_content(&mut state, &mut exec_ctx, &response.content);
                sync_state_to_ctx(ctx, &state);
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
        state_add_output_from_content(&mut state, &mut exec_ctx, &response.content);

        // 9. 检查总输出预算
        if state_exceeded_total_output(&state, self.config.max_total_output_tokens) {
            sync_state_to_ctx(ctx, &state);
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
        if state_exceeded_total_reasoning(&state, self.config.max_total_reasoning_tokens) {
            sync_state_to_ctx(ctx, &state);
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

        // 11. 写入 assistant 响应到 messages
        state_push_assistant(&mut state, &mut exec_ctx, response.content.clone());

        // 12. 检查是否有 tool_calls
        let has_tool_calls = response.has_tool_calls();
        let tool_calls: Vec<ToolCall> = response.tool_calls().cloned().collect();

        if has_tool_calls {
            state_add_tool_calls(&mut state, tool_calls.len());
        }

        // 13. 同步状态到 ctx
        sync_state_to_ctx(ctx, &state);
        ctx.set(SK_HAS_TOOL_CALLS, has_tool_calls);

        if has_tool_calls {
            // 有 tool_calls → 继续循环（到 ToolNode）
            tracing::debug!(
                iteration = get_iterations(&state),
                tool_calls = tool_calls.len(),
                "LLM call completed, executing tools"
            );
        } else {
            // 无 tool_calls → 结束
            ctx.set(SK_STOP_REASON, format!("{:?}", StopReason::Complete));
            ctx.set(
                SK_LAST_RESPONSE,
                serde_json::to_value(&response).unwrap_or_default(),
            );
            ctx.end();
            tracing::debug!(
                iteration = get_iterations(&state),
                "LLM call completed, no tool calls"
            );
        }

        Ok(())
    }
}

// ─── ToolNode ─────────────────────────────────────────────────

/// 工具执行节点 — 读取 tool_calls，执行工具，写入 results。
///
/// 读取 State:
/// - `messages` — 消息历史（用于工具结果截断）
///
/// 写入 State:
/// - `messages` — 追加工具执行结果
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
        let messages = get_messages_from_ctx(ctx);
        let mut state = create_state_from_ctx(ctx);
        let mut exec_ctx = AgentExecutionContext::new(&messages);

        // 1. 获取工具调用
        let round = ResolvedRound::new(self.executor.snapshot().await);
        let last_response: ChatResponse = ctx
            .get_raw(SK_LAST_RESPONSE)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
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

        // 3. 写入工具结果到 messages
        state_push_tool_results(
            &mut state,
            &mut exec_ctx,
            batch.results,
            &self.config.context_budget,
        );

        // 4. 同步状态到 ctx
        sync_state_to_ctx(ctx, &state);

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

// ─── 辅助函数 ─────────────────────────────────────────────────

fn get_messages_from_ctx(ctx: &NodeContext<'_>) -> Vec<Message> {
    ctx.get::<Vec<serde_json::Value>>(SK_MESSAGES)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

fn create_state_from_ctx(ctx: &NodeContext<'_>) -> lellm_graph::State {
    let mut state = lellm_graph::State::new();
    if let Some(v) = ctx.get_raw(SK_MESSAGES) {
        state.insert(SK_MESSAGES.to_string(), v.clone());
    }
    if let Some(v) = ctx.get_raw(SK_ITERATIONS) {
        state.insert(SK_ITERATIONS.to_string(), v.clone());
    }
    if let Some(v) = ctx.get_raw(SK_TOTAL_TOOL_CALLS) {
        state.insert(SK_TOTAL_TOOL_CALLS.to_string(), v.clone());
    }
    if let Some(v) = ctx.get_raw(SK_OUTPUT_TOKENS) {
        state.insert(SK_OUTPUT_TOKENS.to_string(), v.clone());
    }
    if let Some(v) = ctx.get_raw(SK_REASONING_TOKENS) {
        state.insert(SK_REASONING_TOKENS.to_string(), v.clone());
    }
    state
}

fn sync_state_to_ctx(ctx: &mut NodeContext<'_>, state: &lellm_graph::State) {
    for key in [
        SK_MESSAGES,
        SK_ITERATIONS,
        SK_TOTAL_TOOL_CALLS,
        SK_OUTPUT_TOKENS,
        SK_REASONING_TOKENS,
    ] {
        if let Some(v) = state.get(key) {
            ctx.set(key, v.clone());
        }
    }
}

// ─── build_react_graph ────────────────────────────────────────

/// 构建 ReAct 内部图。
///
/// ```text
/// [llm] --has_tool_calls--> [condition] --yes--> [tool] --(self)--> [llm]
///                        \--no--> [end]
/// ```
pub fn build_react_graph(llm_node: LLMNode, tool_node: ToolNode) -> Graph {
    let llm_name = llm_node.name.clone();

    let mut builder = GraphBuilder::new(format!("react_{}", llm_name));
    builder.start("llm");
    builder.end("end");

    builder.node("llm", NodeKind::External(Arc::new(llm_node)));
    builder.node("tool", NodeKind::External(Arc::new(tool_node)));
    builder.node(
        "condition",
        NodeKind::External(Arc::new(ReactCondition::new(format!(
            "{}_condition",
            llm_name
        )))),
    );

    // llm -> condition
    builder.edge("llm", "condition");

    // condition -> tool (有 tool_calls)
    builder.edge_if("condition", "tool", |state| {
        state
            .get(SK_HAS_TOOL_CALLS)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });

    // condition -> end (无 tool_calls)
    builder.edge_fallback("condition", "end");

    // tool -> llm (自环)
    builder.edge("tool", "llm");

    builder.build().expect("ReAct graph should be valid")
}
