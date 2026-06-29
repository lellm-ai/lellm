//! Barrier 等待与决策应用。
//!
//! 从 execution_loop.rs 拆分而来，包含：
//! - `BarrierOutcome` — 等待结果枚举
//! - `wait_for_barrier_decision()` — 等待决策、超时或取消
//! - `apply_barrier_decision_generic()` — 泛型版本（忽略 Modify）
//! - `apply_barrier_decision()` — State 专用版本（完整 Modify 语义）

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

use crate::event::{BarrierDecision, BarrierDecisionMessage, BarrierId};
use crate::state::State;
use crate::workflow_state::WorkflowState;

/// Barrier 等待结果。
pub(crate) enum BarrierOutcome {
    /// 决策已收到
    Decision(BarrierDecision),
    /// 超时 — 应应用默认 Reject
    TimedOut,
    /// 取消
    Cancelled,
}

/// 等待 Barrier 决策、取消或超时。
pub(crate) async fn wait_for_barrier_decision(
    decision_rx: &mut tokio::sync::mpsc::Receiver<BarrierDecisionMessage>,
    cancel_rx: &mut tokio::sync::mpsc::Receiver<()>,
    cancel: &CancellationToken,
    barrier_id: &BarrierId,
    timeout: Option<std::time::Duration>,
    wildcard_cache: &mut HashMap<String, BarrierDecision>,
) -> BarrierOutcome {
    // 先检查通配缓存
    if let Some(decision) = wildcard_cache.get(&barrier_id.node_id) {
        return BarrierOutcome::Decision(decision.clone());
    }

    if let Some(dur) = timeout {
        tokio::select! {
            biased;
            _ = cancel_rx.recv() => {
                cancel.cancel();
                BarrierOutcome::Cancelled
            }
            _ = tokio::time::sleep(dur) => BarrierOutcome::TimedOut,
            msg = decision_rx.recv() => match msg {
                Some(BarrierDecisionMessage::Exact { barrier_id: bid, decision }) => {
                    if bid == *barrier_id { BarrierOutcome::Decision(decision) } else { BarrierOutcome::Cancelled }
                }
                Some(BarrierDecisionMessage::Wildcard { node_id, decision }) => {
                    if node_id == barrier_id.node_id {
                        wildcard_cache.insert(node_id.clone(), decision.clone());
                        BarrierOutcome::Decision(decision)
                    } else { BarrierOutcome::Cancelled }
                }
                None => BarrierOutcome::Cancelled,
            },
        }
    } else {
        tokio::select! {
            biased;
            _ = cancel_rx.recv() => {
                cancel.cancel();
                BarrierOutcome::Cancelled
            }
            msg = decision_rx.recv() => match msg {
                Some(BarrierDecisionMessage::Exact { barrier_id: bid, decision }) => {
                    if bid == *barrier_id { BarrierOutcome::Decision(decision) } else { BarrierOutcome::Cancelled }
                }
                Some(BarrierDecisionMessage::Wildcard { node_id, decision }) => {
                    if node_id == barrier_id.node_id {
                        wildcard_cache.insert(node_id.clone(), decision.clone());
                        BarrierOutcome::Decision(decision)
                    } else { BarrierOutcome::Cancelled }
                }
                None => BarrierOutcome::Cancelled,
            },
        }
    }
}

/// 应用 Barrier 决策到 State。返回 Reroute 目标（如果有）。
///
/// ⚠️ 仅对 `State`（HashMap）有效。Barrier `Modify` 决策依赖 key/value 语义。
/// 泛型版本仅处理 `Approve`/`Reject`/`Reroute`，忽略 `Modify`。
pub(crate) fn apply_barrier_decision_generic<S: WorkflowState>(
    _state: &mut S,
    _node_name: &str,
    decision: &BarrierDecision,
) -> Option<String> {
    match decision {
        BarrierDecision::Approve | BarrierDecision::Reject { .. } => {
            // For generic state, Approve/Reject are no-ops on state.
            // The decision is recorded in the event stream.
            None
        }
        BarrierDecision::Modify { .. } => {
            tracing::warn!(
                "BarrierDecision::Modify is only supported for State (HashMap), ignoring"
            );
            None
        }
        BarrierDecision::Reroute { target } => Some(target.clone()),
    }
}

/// 应用 Barrier 决策到 State。返回 Reroute 目标（如果有）。
///
/// `State` 专用版本，支持完整的 `Modify` 语义。
///
/// ⚠️ 内部执行循环使用泛型版本（不处理 Modify）。
/// 此函数供需要完整 Barrier Modify 语义的外部调用者使用。
#[allow(dead_code)]
pub(crate) fn apply_barrier_decision(
    state: &mut State,
    node_name: &str,
    decision: &BarrierDecision,
) -> Option<String> {
    let approve_key = format!("{node_name}.approved");
    let reject_key = format!("{node_name}.reject_reason");

    match decision {
        BarrierDecision::Approve => {
            state.insert(approve_key, serde_json::json!(true));
            state.remove(&reject_key);
            None
        }
        BarrierDecision::Reject { reason } => {
            state.insert(reject_key, serde_json::json!(reason));
            state.remove(&approve_key);
            None
        }
        BarrierDecision::Modify { key, value } => {
            state.insert(key.clone(), value.clone());
            None
        }
        BarrierDecision::Reroute { target } => Some(target.clone()),
    }
}
