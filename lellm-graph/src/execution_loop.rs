//! Graph 流式执行循环 — SimpleExecutor::execute_stream() 的核心逻辑。
//!
//! 包含：
//! - 执行循环（节点调度、路由、Mutation 消费）
//! - Barrier 等待（决策、超时、取消）
//! - Barrier 决策应用到 State
//! - GraphComplete / GraphError 事件发射
//!
//! v0.4+: 泛型化 `run_execution_loop<S, M>`，支持任意 `WorkflowState`。
//! `apply_barrier_decision` 仍为 `State` 专用（Barrier Modify 依赖 HashMap 语义）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::error::GraphError;
use crate::event::{
    BarrierDecision, BarrierDecisionMessage, GraphEvent,
};
use crate::graph::Graph;
use crate::ids::{SpanId, TraceId};
use crate::node::{ExecutorOperation, FlowNode, NodeKind};
use crate::node_context::{ExecutionEngine, ExecutionSignal, ExecutorState, NextAction};
use crate::state::{ExecutionEntry, GraphResult, State};
use crate::workflow_state::{MergeStrategy, WorkflowState};

/// 运行 Graph 的流式执行循环。
///
/// 在 `tokio::spawn` 中调用，通过 channel 发射 `GraphEvent`。
///
/// # 泛型
///
/// - `S` — 类型化状态
/// - `M` — 合并策略
pub(crate) async fn run_execution_loop<S, M>(
    graph: Arc<Graph<S, M>>,
    state: S,
    max_steps: usize,
    trace_id: TraceId,
    event_tx: tokio::sync::mpsc::Sender<GraphEvent<S>>,
    mut decision_rx: tokio::sync::mpsc::Receiver<BarrierDecisionMessage>,
    mut cancel_rx: tokio::sync::mpsc::Receiver<()>,
    cancel: CancellationToken,
) where
    S: WorkflowState + Clone + Send + Sync + 'static,
    M: MergeStrategy<S>,
{
    let start_time = Instant::now();
    let mut execution_log: Vec<ExecutionEntry> = Vec::new();
    let mut engine = ExecutionEngine::new(state, None, cancel.clone());
    let mut current = graph.start_node().to_string();
    let mut step: usize = 0;
    // 缓存通配决策 — 一次发送，多次匹配
    let mut wildcard_cache: HashMap<String, BarrierDecision> = HashMap::new();

    let _ = event_tx.send(GraphEvent::GraphStart { trace_id }).await;

    loop {
        if cancel.is_cancelled() {
            let _ = event_tx
                .send(GraphEvent::GraphError {
                    error: GraphError::Terminal(crate::error::TerminalError::BarrierCancelled {
                        node: "execution cancelled".into(),
                    }),
                    state: engine.state().clone(),
                })
                .await;
            break;
        }

        step += 1;
        if step > max_steps {
            let _ = event_tx
                .send(GraphEvent::GraphError {
                    error: GraphError::Terminal(crate::error::TerminalError::StepsExceeded {
                        limit: max_steps,
                    }),
                    state: engine.state().clone(),
                })
                .await;
            break;
        }

        let node = match graph.nodes.get(&current) {
            Some(n) => n,
            None => {
                let _ = event_tx
                    .send(GraphEvent::GraphError {
                        error: GraphError::Terminal(
                            crate::error::TerminalError::NodeNotFound(current.clone()),
                        ),
                        state: engine.state().clone(),
                    })
                    .await;
                break;
            }
        };

        let node_name = current.clone();
        let span_id = SpanId::new();
        let node_start = Instant::now();

        let _ = event_tx
            .send(GraphEvent::NodeStart {
                node_name: node_name.clone(),
                trace_id,
                span_id,
                step,
            })
            .await;

        // 执行节点 — 根据 NodeKind 分发
        let node_ok = match node {
            NodeKind::Task(n) => {
                let mut ctx = engine.build_node_context();
                n.execute(&mut ctx).await.is_ok()
            }
            NodeKind::Condition(n) => {
                let mut ctx = engine.build_node_context();
                n.execute(&mut ctx).await.is_ok()
            }
            NodeKind::Barrier(n) => {
                let mut ctx = engine.build_node_context();
                n.execute(&mut ctx).await.is_ok()
            }
            NodeKind::External(n) => {
                let mut ctx = engine.build_node_context();
                n.execute(&mut ctx).await.is_ok()
            }
            NodeKind::ExternalLeaf(n) => {
                let mut ctx = engine.build_leaf_context();
                n.execute(&mut ctx).await.is_ok()
            }
            NodeKind::Parallel(p) => p.execute(&mut engine).await.is_ok(),
        };

        if !node_ok {
            let _ = event_tx
                .send(GraphEvent::NodeEnd {
                    node_name: node_name.clone(),
                    trace_id,
                    span_id,
                    success: false,
                    duration: node_start.elapsed(),
                })
                .await;

            let _ = event_tx
                .send(GraphEvent::GraphError {
                    error: GraphError::Terminal(
                        crate::error::TerminalError::NodeExecutionFailed {
                            node: node_name,
                            source: "node execution failed".into(),
                        },
                    ),
                    state: engine.state().clone(),
                })
                .await;
            break;
        }

        // 消费 FlowEvent 缓冲 → 转发为 GraphEvent::Node
        let flow_events = engine.take_flow_events();
        for fe in flow_events {
            let _ = event_tx
                .send(GraphEvent::Node {
                    span_id,
                    node_name: node_name.clone(),
                    event: fe,
                })
                .await;
        }

        // commit mutations (Unit of Work) — 对 Parallel 是空操作
        engine.commit();

        let node_duration = node_start.elapsed();
        execution_log.push(ExecutionEntry {
            step,
            node_name: node_name.clone(),
            start_time,
            end_time: start_time.checked_add(node_duration).unwrap_or(start_time),
            success: true,
            error: None,
        });

        let _ = event_tx
            .send(GraphEvent::NodeEnd {
                node_name: node_name.clone(),
                trace_id,
                span_id,
                success: true,
                duration: node_duration,
            })
            .await;

        // 提取控制信号
        let (next_action, signal) = engine.take_control();

        // 处理 Barrier 信号
        let mut next_action = next_action;

        if let Some(ExecutionSignal::Pause {
            barrier_id,
            timeout,
        }) = signal
        {
            let _ = event_tx
                .send(GraphEvent::BarrierWaiting {
                    barrier_id: barrier_id.clone(),
                    node_name: node_name.clone(),
                    span_id,
                })
                .await;

            let outcome = wait_for_barrier_decision(
                &mut decision_rx,
                &mut cancel_rx,
                &cancel,
                &barrier_id,
                timeout,
                &mut wildcard_cache,
            )
            .await;

            match outcome {
                BarrierOutcome::Decision(d) => {
                    let _ = event_tx
                        .send(GraphEvent::BarrierResolved {
                            barrier_id,
                            decision: d.clone(),
                        })
                        .await;

                    let reroute_target = apply_barrier_decision_generic(
                        engine.state_mut(),
                        &node_name,
                        &d,
                    );

                    if let Some(target) = reroute_target {
                        current = target;
                        continue;
                    }
                    next_action = NextAction::Next;
                }
                BarrierOutcome::TimedOut => {
                    // 超时 → 应用默认 Reject 决策
                    let _ = event_tx
                        .send(GraphEvent::BarrierResolved {
                            barrier_id,
                            decision: BarrierDecision::Reject {
                                reason: "timeout".into(),
                            },
                        })
                        .await;
                    apply_barrier_decision_generic(
                        engine.state_mut(),
                        &node_name,
                        &BarrierDecision::Reject { reason: "timeout".into() },
                    );
                    next_action = NextAction::Next;
                }
                BarrierOutcome::Cancelled => {
                    let _ = event_tx
                        .send(GraphEvent::GraphError {
                            error: GraphError::Terminal(
                                crate::error::TerminalError::BarrierCancelled {
                                    node: node_name.clone(),
                                },
                            ),
                            state: engine.state().clone(),
                        })
                        .await;
                    break;
                }
            }
        }

        // 处理路由
        match next_action {
            NextAction::End => {
                send_complete(
                    &event_tx,
                    trace_id,
                    engine,
                    execution_log,
                    start_time,
                );
                break;
            }
            NextAction::Goto(target) => {
                current = target;
            }
            NextAction::Next => {
                if current == graph.end_node() {
                    send_complete(
                        &event_tx,
                        trace_id,
                        engine,
                        execution_log,
                        start_time,
                    );
                    break;
                }
                match graph.resolve_next_inline(&current, engine.state()) {
                    Ok(target) => current = target,
                    Err(e) => {
                        let _ = event_tx
                            .send(GraphEvent::GraphError {
                                error: e,
                                state: engine.state().clone(),
                            })
                            .await;
                        break;
                    }
                }
            }
        }
    }
}

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
    barrier_id: &crate::event::BarrierId,
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

/// 发送 GraphComplete 事件。
pub(crate) fn send_complete<S: WorkflowState>(
    event_tx: &tokio::sync::mpsc::Sender<GraphEvent<S>>,
    trace_id: TraceId,
    engine: ExecutionEngine<S>,
    execution_log: Vec<ExecutionEntry>,
    start_time: Instant,
) {
    let duration = start_time.elapsed();
    let final_state = engine.into_state();
    let result = GraphResult {
        trace_id,
        state: final_state,
        execution_log,
        duration,
    };
    let _ = event_tx.try_send(GraphEvent::GraphComplete { result });
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
