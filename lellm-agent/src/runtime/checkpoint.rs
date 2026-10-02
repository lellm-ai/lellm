//! Agent Checkpoint Phase 2 — 持久化执行 + 恢复入口。
//!
//! 复用第一阶段 inline checkpoint 基础设施：
//! - `invoke_with_checkpoint` — 首次执行（trace 必须新鲜）。
//! - `invoke_with_restore` — 从检查点恢复（分层校验 + 续跑）。
//!
//! 错误映射：恢复**校验**失败 → `LlmError::RestoreFailed{reason}`；
//! 运行期错误（含保存失败）→ `LlmError::Provider`。

use lellm_core::{ChatResponse, ContentBlock, LlmError, Message, RestoreFailureReason, TokenUsage};
use lellm_graph::exec::CheckpointSaveSink;
use lellm_graph::{
    CancellationToken, Checkpoint, CheckpointConfig, ExecutionContext, GraphError,
    NoopStepCallback, TerminalError, TraceId,
};

use super::ToolUseResult;
use super::config::{ToolUseConfig, build_request_messages_inner, empty_response};
use super::event::StopReason;
use super::runtime::ToolUseLoop;
use super::typed_state::AgentState;

// ─── 辅助函数 ───────────────────────────────────────────────────

/// 图步数预算 — 与 `invoke` 一致（每轮 ReAct 最坏 4 steps + 1 buffer）。
pub(crate) fn max_steps_for(config: &ToolUseConfig) -> usize {
    config.max_iterations * 4 + 1
}

/// 从 AgentState 构造 ToolUseResult。
///
/// 完成态结果构造契约（§5.4）：`last_response` 缺失（旧检查点）时
/// 从 messages 的最后一条 Assistant 消息重建 response。
pub(crate) fn build_result(state: &AgentState) -> ToolUseResult {
    let stop_reason = state.stop_reason.clone().unwrap_or(StopReason::Complete);
    let response = state
        .last_response
        .clone()
        .unwrap_or_else(|| reconstruct_response_from_messages(&state.messages));
    ToolUseResult {
        stop_reason,
        response,
        messages: state.messages.clone(),
        iterations: state.iterations,
        tool_calls_executed: state.total_tool_calls,
    }
}

/// 从 messages 的最后一条 Assistant 消息重建 ChatResponse（§5.4）。
pub(crate) fn reconstruct_response_from_messages(messages: &[Message]) -> ChatResponse {
    match messages.iter().rev().find_map(|m| match m {
        Message::Assistant { content } => Some(content.clone()),
        _ => None,
    }) {
        Some(content) => {
            let text = ContentBlock::flatten_text(&content);
            ChatResponse::new(
                lellm_core::text_block(text),
                TokenUsage::default(),
                serde_json::Value::Null,
            )
        }
        None => empty_response(),
    }
}

/// Agent 层校验 — 恢复目标节点对 last_response 的输入完整性（§5.2）。
///
/// `post_llm_check` / `tool` 需要 `last_response = Some`，否则 `MissingExecutionContext`；
/// `budget_check` / `llm` / `compactor` / `end` / `None` 允许 `None`。
pub(crate) fn validate_last_response(cp: &Checkpoint<AgentState>) -> Result<(), LlmError> {
    match cp.next_node.as_ref().map(|n| n.0.as_str()) {
        Some("post_llm_check") | Some("tool") if cp.state.last_response.is_none() => {
            Err(LlmError::RestoreFailed {
                reason: RestoreFailureReason::MissingExecutionContext,
                message: format!(
                    "node '{}' requires last_response but checkpoint has none (old checkpoint?)",
                    cp.next_node.as_ref().unwrap().0
                ),
            })
        }
        _ => Ok(()),
    }
}

/// 恢复**校验**阶段错误 → `LlmError::RestoreFailed{reason}`。
///
/// 承接 4 个校验步骤：`validate_persistable`（`RestoreUnsupported` → catch-all → `Other`）、
/// `validate_restore_checkpoint`、`check_restore_latest`、`validate_last_response`。
pub(crate) fn map_restore_error(e: GraphError) -> LlmError {
    match e {
        GraphError::Terminal(TerminalError::RestoreFailed { reason }) => {
            let r = if reason.contains("format_version") {
                RestoreFailureReason::UnsupportedFormat
            } else if reason.contains("hash mismatch") {
                RestoreFailureReason::GraphMismatch
            } else {
                RestoreFailureReason::Other
            };
            LlmError::RestoreFailed {
                reason: r,
                message: reason,
            }
        }
        GraphError::Terminal(TerminalError::RestoreNotLatest { checkpoint, latest }) => {
            LlmError::RestoreFailed {
                reason: RestoreFailureReason::NotLatest,
                message: format!("checkpoint {checkpoint} is not the latest ({latest})"),
            }
        }
        GraphError::Terminal(TerminalError::NodeNotFound(name)) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::NodeNotFound,
            message: format!("next node not found: {name}"),
        },
        GraphError::Terminal(TerminalError::StepsExceeded { limit }) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::StepsExceeded,
            message: format!("step budget exhausted (limit {limit})"),
        },
        GraphError::Terminal(other) => LlmError::RestoreFailed {
            reason: RestoreFailureReason::Other,
            message: format!("{other:?}"),
        },
    }
}

/// 运行期错误（含保存失败）→ `LlmError::Provider`（非恢复失败）。
pub(crate) fn map_runtime_error(e: GraphError) -> LlmError {
    LlmError::Provider {
        provider: "react_graph".into(),
        status: None,
        code: None,
        message: e.to_string(),
    }
}

// ─── ToolUseLoop 入口 ───────────────────────────────────────────

impl ToolUseLoop {
    /// 非流式首次执行（带 checkpoint 保存）。
    ///
    /// `trace_id` 由调用方预提供；要求该 trace **无既有检查点**（新鲜），
    /// 否则报错提示改用 `invoke_with_restore`。
    pub async fn invoke_with_checkpoint(
        &self,
        messages: Vec<Message>,
        trace_id: TraceId,
        config: CheckpointConfig<AgentState>,
    ) -> Result<ToolUseResult, LlmError> {
        // 首次执行绑定规则：trace 必须新鲜
        config
            .assert_fresh_trace(&trace_id)
            .await
            .map_err(|e| LlmError::InvalidRequest {
                message: format!(
                    "trace {trace_id} already has checkpoints; use invoke_with_restore: {e}"
                ),
            })?;

        let initial_messages = build_request_messages_inner(self.config(), &messages)?;
        let max_steps = max_steps_for(self.config());
        let mut state = AgentState::from_messages(initial_messages);

        let mut sink = CheckpointSaveSink::new(config, trace_id, None);
        let mut step_cb = NoopStepCallback;
        let mut engine = ExecutionContext::new(
            &mut state,
            None,
            CancellationToken::new(),
            Some(&mut sink),
            None,
        );
        self.graph()
            .run_inline(&mut engine, max_steps, &mut step_cb)
            .await
            .map_err(map_runtime_error)?;
        // 经 engine 取状态（与 invoke 一致，避免与 engine 持有的 &mut state 冲突）
        Ok(build_result(engine.state()))
    }

    /// 非流式恢复执行 — 从检查点续跑，继续保存到同一 trace。
    ///
    /// 分层校验：
    /// 1. 图可持久化（`validate_persistable`）
    /// 2. Graph 泛型校验（版本/指纹/节点存在/步数边界）
    /// 3. 最新性（`check_restore_latest`，store-backed）
    /// 4. Agent 层校验（`validate_last_response`）
    /// 完成态（`next_node = None`）零执行，直接从 state 构造结果（§5.4）。
    pub async fn invoke_with_restore(
        &self,
        checkpoint: Checkpoint<AgentState>,
        trace_id: TraceId,
        config: CheckpointConfig<AgentState>,
    ) -> Result<ToolUseResult, LlmError> {
        let max_steps = max_steps_for(self.config());
        // 恢复校验层（4 步）— 全部映射 RestoreFailed：
        // ① 图可持久化（无 Parallel/Subgraph/Barrier）
        self.graph()
            .validate_persistable()
            .map_err(map_restore_error)?;
        // ② Graph 泛型校验（版本/指纹/节点存在/步数边界）
        self.graph()
            .validate_restore_checkpoint(&checkpoint, max_steps)
            .map_err(map_restore_error)?;
        config
            .check_restore_latest(&trace_id, &checkpoint)
            .await
            .map_err(map_restore_error)?;
        validate_last_response(&checkpoint)?;

        // 先取出 next_node / steps_used（restore_state 会消费 checkpoint）
        let next_node = checkpoint.next_node.clone();
        let steps_used = checkpoint.steps_used;
        let mut state = checkpoint.restore_state();

        // 完成态：零执行，直接从 state 构造结果（§5.4 契约）
        let next_node = match next_node {
            Some(n) => n,
            None => return Ok(build_result(&state)),
        };

        let mut sink = CheckpointSaveSink::new(config, trace_id, None);
        let mut step_cb = NoopStepCallback;
        let mut engine = ExecutionContext::new(
            &mut state,
            None,
            CancellationToken::new(),
            Some(&mut sink),
            None,
        );
        self.graph()
            .run_inline_from(
                &mut engine,
                &next_node.0,
                steps_used,
                max_steps,
                &mut step_cb,
            )
            .await
            .map_err(map_runtime_error)?;
        // 经 engine 取状态（与 invoke 一致，避免与 engine 持有的 &mut state 冲突）
        Ok(build_result(engine.state()))
    }
}
