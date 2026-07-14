//! Guard 节点 — 终止条件检查、上下文压缩、预算判断。
//!
//! 包含 PostLLMGuard（从 LLMNode 提取的运行时策略）、
//! CompactorNode（上下文压缩）、BudgetCondition（预算判断）。

use std::sync::Arc;

use async_trait::async_trait;

use lellm_core::ContentBlock;
use lellm_graph::{GraphError, LeafContext, LeafNode};

use super::super::config::{ToolUseConfig, empty_response};
use super::super::context::{ContextBudget, ContextCompactor, estimate_reasoning_block};
use super::super::event::StopReason;
use super::super::typed_state::{AgentMutation, AgentState};

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
impl LeafNode<AgentState> for PostLLMGuard {
    async fn execute(&self, ctx: &mut LeafContext<'_, AgentState>) -> Result<(), GraphError> {
        // 只 clone last_response（借用跨度长，与 ctx.record 冲突）
        // 其余字段用 Copy 值或引用
        let state = ctx.state();
        let is_terminal = state.stop_reason.is_some();
        let iterations = state.iterations;
        let output_tokens = state.output_tokens;
        let reasoning_tokens = state.reasoning_tokens;
        // last_response 必须 clone：借用跨度覆盖多个 ctx.record/goto/end 调用
        let last_response = state.last_response.clone().unwrap_or_else(empty_response);

        // 1. 已终止（前置节点已设置 stop_reason）→ End
        if is_terminal {
            ctx.end();
            return Ok(());
        }

        // 2. 超过最大迭代 → End
        if iterations >= self.stop_config.max_iterations {
            ctx.record(AgentMutation::SetStopReason(
                StopReason::MaxIterationsReached,
            ));
            ctx.end();
            return Ok(());
        }

        // 3-5. Budget 检查（优先级：单轮推理 > 总输出 > 总推理）
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
                ctx.record(AgentMutation::SetStopReason(
                    StopReason::ReasoningBudgetExceeded,
                ));
                stopped = true;
            }
        }

        // 4. 总输出 Token 超限
        if !stopped {
            if let Some(max) = self.stop_config.max_total_output_tokens {
                if output_tokens >= max as usize {
                    ctx.record(AgentMutation::SetStopReason(
                        StopReason::OutputBudgetExceeded,
                    ));
                    stopped = true;
                }
            }
        }

        // 5. 总推理 Token 超限
        if !stopped {
            if let Some(max) = self.stop_config.max_total_reasoning_tokens {
                if reasoning_tokens >= max as usize {
                    ctx.record(AgentMutation::SetStopReason(
                        StopReason::ReasoningBudgetExceeded,
                    ));
                }
            }
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
        ctx.record(AgentMutation::SetStopReason(StopReason::Complete));
        ctx.end();

        Ok(())
    }
}

// ─── CompactorNode ────────────────────────────────────────────

/// 上下文压缩节点 — 独立 FlowNode，职责单一。
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
impl LeafNode<AgentState> for CompactorNode {
    async fn execute(&self, ctx: &mut LeafContext<'_, AgentState>) -> Result<(), GraphError> {
        let state = ctx.state();

        if !self.budget.should_compact(state.estimated_context_tokens()) {
            return Ok(());
        }

        let result = self.compactor.compact(&state.messages, &self.budget);

        // 只有实际压缩了才 emit Effects
        if result.removed_messages > 0 {
            ctx.record(AgentMutation::ReplaceMessages(result.messages));
            ctx.record(AgentMutation::IncrementCompactCount);

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
impl LeafNode<AgentState> for BudgetCondition {
    async fn execute(&self, ctx: &mut LeafContext<'_, AgentState>) -> Result<(), GraphError> {
        let state = ctx.state();

        if self.budget.should_compact(state.estimated_context_tokens()) {
            ctx.goto("compactor");
        } else {
            ctx.goto("llm");
        }

        Ok(())
    }
}
