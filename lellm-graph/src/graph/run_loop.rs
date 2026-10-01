//! Graph 执行循环本体 — run_inline / run_inline_from 共享。
//!
//! 顺序（每步）：
//! ```text
//! step = steps_used 延续，checked_add(1)（溢出安全）
//! step > max_steps（总预算）→ StepsExceeded
//! execute(node) → on_node_end → 错误传播
//! commit()
//! take_control() → (next_action, signal)
//! [Barrier Pause → wait → outcome → 路由覆盖]
//! 解析 next: Option<NodeId>（End/到达 end 节点 → None；Goto/Next → Some）
//! emit_checkpoint(next, step).await?   ★ 同步保存，失败 → Err，不越过边界
//! next = None → return Ok；Some(n) → current = n
//! ```

use std::time::Instant;

use super::graph_core::{Graph, StepCallback};
use crate::checkpoint::NodeId;
use crate::error::{GraphError, TerminalError};
use crate::exec::execution_engine::{ExecutionEngine, ExecutionSignal, ExecutorState, NextAction};
use crate::ids::SpanId;
#[allow(deprecated)]
use crate::node::{BarrierNode, ConditionNode, FlowNode, LeafNode, NodeKind};
use crate::state::workflow_state::{MergeStrategy, WorkflowState};

/// 执行循环本体（pub(crate) — 由 Graph::run_inline_from 调用）。
#[allow(deprecated)]
pub(crate) async fn run_graph_loop<'cb, S, M>(
    graph: &Graph<S, M>,
    exec_ctx: &mut ExecutionEngine<'_, S>,
    start_node: &str,
    steps_used: usize,
    max_steps: usize,
    step_cb: &mut dyn StepCallback<'cb>,
) -> Result<(), GraphError>
where
    S: WorkflowState,
    M: MergeStrategy<S>,
{
    let mut current = start_node.to_string();
    let mut step = steps_used;

    loop {
        // 溢出安全预算检查（max_steps = 总预算；恢复时从 steps_used 延续）
        step = step.checked_add(1).ok_or_else(|| {
            GraphError::Terminal(TerminalError::StepsExceeded { limit: max_steps })
        })?;
        if step > max_steps {
            return Err(GraphError::Terminal(TerminalError::StepsExceeded {
                limit: max_steps,
            }));
        }

        let node = graph
            .nodes
            .get(&current)
            .ok_or_else(|| GraphError::Terminal(TerminalError::NodeNotFound(current.clone())))?;

        let node_start = Instant::now();
        let span_id = SpanId::new();
        step_cb.on_node_start(&current, span_id, step);

        let exec_result = match node {
            NodeKind::Task(n) => {
                let mut ctx = exec_ctx.build_node_context();
                n.execute(&mut ctx).await
            }
            NodeKind::Condition(n) => {
                let mut ctx = exec_ctx.build_leaf_context();
                <ConditionNode<S> as LeafNode<S>>::execute(n, &mut ctx).await
            }
            NodeKind::Barrier(n) => {
                let mut ctx = exec_ctx.build_leaf_context();
                <BarrierNode<S> as LeafNode<S>>::execute(n, &mut ctx).await
            }
            NodeKind::External(n) => {
                let mut ctx = exec_ctx.build_node_context();
                n.execute(&mut ctx).await
            }
            NodeKind::ExternalLeaf(n) => {
                let mut ctx = exec_ctx.build_leaf_context();
                n.execute(&mut ctx).await
            }
            NodeKind::Parallel(p) => p.execute(exec_ctx).await,
            NodeKind::Subgraph(spec) => {
                let stream = exec_ctx.stream_sink();
                let cancel = exec_ctx.cancel_token().clone();
                spec.execute(exec_ctx.state_mut(), stream, cancel).await
            }
        };

        let node_duration = node_start.elapsed();
        let success = exec_result.is_ok();
        step_cb.on_node_end(&current, span_id, step, node_duration, success);
        exec_result?;

        // commit → take_control → 解析 next → ★ 同步保存
        exec_ctx.commit();
        let (next_action, signal) = exec_ctx.take_control();

        let next = if let Some(ExecutionSignal::Pause {
            barrier_id,
            timeout,
        }) = signal
        {
            let span_id = SpanId::new();
            step_cb.on_barrier_waiting(&barrier_id, &current, span_id);
            let outcome = exec_ctx.wait_barrier(&barrier_id, timeout).await;

            let barrier = match node {
                NodeKind::Barrier(b) => b,
                _ => unreachable!("Pause 信号仅由 Barrier 节点产生"),
            };

            // 归约「是否拒绝」：显式 Reject，或超时且 default_action=Reject
            let rejected = match &outcome {
                crate::node::barrier_sink::BarrierOutcome::Decision(
                    crate::event::BarrierDecision::Reject { .. },
                ) => true,
                crate::node::barrier_sink::BarrierOutcome::TimedOut => matches!(
                    barrier.default_action,
                    crate::node::BarrierDefaultAction::Reject
                ),
                _ => false,
            };

            if rejected {
                // 拒绝：有 reject_target → 跳转；无 → 完成（保存完成态检查点）
                barrier.reject_target.as_ref().map(|t| NodeId(t.clone()))
            } else {
                match outcome {
                    crate::node::barrier_sink::BarrierOutcome::Decision(
                        crate::event::BarrierDecision::Reroute { target },
                    ) => Some(NodeId(target)),
                    crate::node::barrier_sink::BarrierOutcome::Cancelled => {
                        return Err(GraphError::Terminal(TerminalError::BarrierCancelled {
                            node: current.clone(),
                        }));
                    }
                    // Approve / Modify / Timeout(Approve|Skip) — 正常路由
                    _ => resolve_next_opt(graph, &next_action, &current, exec_ctx.state())?,
                }
            }
        } else {
            resolve_next_opt(graph, &next_action, &current, exec_ctx.state())?
        };

        // ★ 同步保存检查点：失败 → 错误，不越过边界
        exec_ctx.emit_checkpoint(next.clone(), step).await?;

        match next {
            None => return Ok(()),
            Some(n) => current = n.0,
        }
    }
}

/// 路由解析 — NextAction + current → Option<NodeId>（None = 已完成）。
fn resolve_next_opt<S: WorkflowState>(
    graph: &Graph<S, impl MergeStrategy<S>>,
    next_action: &NextAction,
    current: &str,
    state: &S,
) -> Result<Option<NodeId>, GraphError> {
    Ok(match next_action {
        NextAction::End => None,
        NextAction::Goto(target) => Some(NodeId(target.clone())),
        NextAction::Next => {
            if current == graph.end_node() {
                None
            } else {
                Some(NodeId(graph.resolve_next_inline(current, state)?))
            }
        }
    })
}
