//! AgentState 并行合并策略 — 基于 `base` 的 delta 合并（对齐 `StateMerge`）。
//!
//! 从 `typed_state` 模块拆出，使该文件聚焦于状态定义本身。

use lellm_core::{ChatResponse, Message};

use super::event::StopReason;
use super::typed_state::AgentState;

/// AgentState 的默认合并策略（基于 `base` 的 delta 合并，对齐 `StateMerge`）。
///
/// 以分支执行前的父状态 `base` 为基准，只合并各分支的**实际变更**；对不兼容的并发
/// 写入返回 [`lellm_graph::WorkflowError::MergeConflict`]（不静默猜测）。逐字段规则：
///
/// - `messages`：各分支「追加后缀」按注册顺序拼接（`base + suffixes`）；某分支非纯追加
///   （压缩/清空/截断 = replace）且另有分支非空追加 → 冲突；仅某分支 replace 且其余
///   未动 → 取该 replace。
/// - `iterations`：全未变 → base；单分支变 → 取该值；多分支全 ≥ base → `max`；任一
///   < base → 冲突。
/// - 累加器（`total_tool_calls`/`output_tokens`/`reasoning_tokens`/`compact_count`）：
///   全未变 → base；单分支变（含清零/降低）→ 取该值；多分支全 ≥ base → `base + Σ增量`
///   （checked 加法，溢出报错）；任一 < base → 冲突。
/// - `stop_reason` + `last_response`（成组，因 stop_reason 由 last_response 派生）：
///   0 分支变 → base；单分支变 → 取该分支两者；多分支变 → 冲突（不拼接从未出现的终态）。
#[derive(Clone)]
pub struct AgentStateMerge;

/// 数值计数器在多分支全 ≥ base 时的合并方式。
#[derive(Clone, Copy)]
enum CounterCombine {
    /// 累加器：`base + Σ(分支 − base)`。
    Sum,
    /// 轮次：`max(base, 各分支)`。
    Max,
}

impl lellm_graph::MergeStrategy<AgentState> for AgentStateMerge {
    fn merge(
        base: &AgentState,
        branches: Vec<AgentState>,
    ) -> Result<AgentState, lellm_graph::WorkflowError> {
        // 空分支 → base（无并行工作 = 无变更）
        if branches.is_empty() {
            return Ok(base.clone());
        }

        let messages = merge_messages(&base.messages, &branches)?;
        let iterations = merge_counter(
            base.iterations,
            &branches,
            |b| b.iterations,
            CounterCombine::Max,
            "iterations",
        )?;
        let total_tool_calls = merge_counter(
            base.total_tool_calls,
            &branches,
            |b| b.total_tool_calls,
            CounterCombine::Sum,
            "total_tool_calls",
        )?;
        let output_tokens = merge_counter(
            base.output_tokens,
            &branches,
            |b| b.output_tokens,
            CounterCombine::Sum,
            "output_tokens",
        )?;
        let reasoning_tokens = merge_counter(
            base.reasoning_tokens,
            &branches,
            |b| b.reasoning_tokens,
            CounterCombine::Sum,
            "reasoning_tokens",
        )?;
        let compact_count = merge_counter(
            base.compact_count,
            &branches,
            |b| b.compact_count,
            CounterCombine::Sum,
            "compact_count",
        )?;
        let (stop_reason, last_response) = merge_terminal_pair(base, &branches)?;

        Ok(AgentState {
            messages,
            iterations,
            total_tool_calls,
            output_tokens,
            reasoning_tokens,
            compact_count,
            stop_reason,
            last_response,
        })
    }

    fn default_instance() -> Self {
        AgentStateMerge
    }
}

/// 合并 messages：追加后缀按注册顺序拼接；replace 冲突。
fn merge_messages(
    base: &[Message],
    branches: &[AgentState],
) -> Result<Vec<Message>, lellm_graph::WorkflowError> {
    let mut n_replace = 0usize;
    let mut n_nonempty_append = 0usize;
    let mut replace_msgs: Option<Vec<Message>> = None;

    for b in branches {
        let bm = &b.messages;
        // 「纯追加」判定：base 是 bm 的完整前缀（长度 ≥ base 且前缀逐元素相等）
        let is_append = bm.len() >= base.len() && &bm[..base.len()] == base;
        if is_append {
            if bm.len() > base.len() {
                n_nonempty_append += 1;
            }
        } else {
            n_replace += 1;
            replace_msgs = Some(bm.clone());
        }
    }

    if n_replace == 0 {
        // 全追加：base + 各后缀（注册顺序）
        let mut merged = base.to_vec();
        for b in branches {
            let bm = &b.messages;
            merged.extend(bm[base.len()..].iter().cloned());
        }
        Ok(merged)
    } else if n_replace == 1 && n_nonempty_append == 0 {
        // 恰好 1 个替换、其余未变 → 取该替换
        Ok(replace_msgs.expect("n_replace == 1 时必有替换消息"))
    } else {
        Err(lellm_graph::WorkflowError::MergeConflict(
            "parallel merge conflict: messages replaced by multiple branches or mixed with appends"
                .into(),
        ))
    }
}

/// 合并数值计数器（累加器求和 / 轮次取 max），reset 与溢出单独处理。
fn merge_counter(
    base: usize,
    branches: &[AgentState],
    get: impl Fn(&AgentState) -> usize,
    combine: CounterCombine,
    field: &str,
) -> Result<usize, lellm_graph::WorkflowError> {
    let values: Vec<usize> = branches.iter().map(|b| get(b)).collect();
    let changed = values.iter().filter(|v| **v != base).count();

    match changed {
        0 => Ok(base),
        1 => Ok(values
            .iter()
            .find(|v| **v != base)
            .copied()
            .expect("changed == 1 时必有变更值")),
        _ => {
            // 多分支变更：任一 < base（reset）→ 冲突
            if values.iter().any(|v| *v < base) {
                return Err(lellm_graph::WorkflowError::MergeConflict(format!(
                    "parallel merge conflict: counter '{field}' reset by multiple branches"
                )));
            }
            match combine {
                CounterCombine::Sum => {
                    let mut sum = base;
                    for v in &values {
                        sum = sum.checked_add(v - base).ok_or_else(|| {
                            lellm_graph::WorkflowError::MergeConflict(format!(
                                "counter '{field}' overflow during parallel merge"
                            ))
                        })?;
                    }
                    Ok(sum)
                }
                CounterCombine::Max => Ok(values.iter().copied().max().expect("branches 非空")),
            }
        }
    }
}

/// 合并 (stop_reason, last_response) 组：单写者保留，多写者冲突。
///
/// 成组处理是因为 stop_reason 由 last_response 派生（PostLLMGuard 检查 last_response
/// 决定停止原因）——分别取自不同分支会拼出从未出现过的终态。
fn merge_terminal_pair(
    base: &AgentState,
    branches: &[AgentState],
) -> Result<(Option<StopReason>, Option<ChatResponse>), lellm_graph::WorkflowError> {
    let changed: Vec<&AgentState> = branches
        .iter()
        .filter(|b| b.stop_reason != base.stop_reason || b.last_response != base.last_response)
        .collect();

    match changed.len() {
        0 => Ok((base.stop_reason.clone(), base.last_response.clone())),
        1 => Ok((
            changed[0].stop_reason.clone(),
            changed[0].last_response.clone(),
        )),
        _ => Err(lellm_graph::WorkflowError::MergeConflict(
            "parallel merge conflict: stop_reason/last_response written by multiple branches"
                .into(),
        )),
    }
}
