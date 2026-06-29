//! Graph 流式执行循环 — SimpleExecutor::execute_stream() 的核心逻辑。
//!
//! 包含：
//! - 执行循环（节点调度、路由、Mutation 消费）
//! - GraphComplete / GraphError 事件发射
//! - Checkpoint Save Path（根据 CheckpointPolicy 触发）
//!
//! Barrier 等待与决策应用见 [`barrier_wait`] 模块。
//! Checkpoint Restore 路径留给 v0.5。
//!
//! v0.4+: 泛型化 `run_execution_loop<S, M>`，支持任意 `WorkflowState`。

use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::barrier_wait::{
    BarrierOutcome, apply_barrier_decision_generic, wait_for_barrier_decision,
};
use crate::checkpoint::{Checkpoint, CheckpointPolicy, TraceId};
use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierDecisionMessage, GraphEvent};
use crate::execution_engine::{ExecutionEngine, ExecutionSignal, ExecutorState, NextAction};
use crate::graph::Graph;
use crate::ids::SpanId;
use crate::node::{BarrierNode, ConditionNode, ExecutorOperation, FlowNode, LeafNode, NodeKind};
use crate::state::{ExecutionEntry, GraphResult};
use crate::workflow_state::{MergeStrategy, WorkflowState};

// ─── CheckpointConfig ─────────────────────────────────────────

/// Checkpoint 保存回调 — 传入 `run_execution_loop` 即可启用自动保存。
///
/// v0.4 只做 save path，restore 留给 v0.5。
type CheckpointSaveFn<S> = Box<
    dyn Fn(
            Checkpoint<S>,
            TraceId,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<(), crate::checkpoint::CheckpointStoreError>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// Checkpoint 保存配置。
pub struct CheckpointConfig<S: WorkflowState> {
    save_fn: CheckpointSaveFn<S>,
    policy: CheckpointPolicy,
    graph_hash: u64,
}

impl<S: WorkflowState> CheckpointConfig<S> {
    /// 创建 CheckpointConfig。
    ///
    /// `save_fn` 接收 `(Checkpoint<S>, TraceId)` 并异步保存。
    /// 通常由调用方组合 `TypedCheckpointStore` + `SerdeCheckpointCodec` 构造。
    pub fn new(
        save_fn: impl Fn(
            Checkpoint<S>,
            TraceId,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<(), crate::checkpoint::CheckpointStoreError>,
                    > + Send,
            >,
        > + Send
        + Sync
        + 'static,
        graph_hash: u64,
    ) -> Self {
        Self {
            save_fn: Box::new(save_fn),
            policy: CheckpointPolicy::default(),
            graph_hash,
        }
    }

    pub fn with_policy(mut self, policy: CheckpointPolicy) -> Self {
        self.policy = policy;
        self
    }
}

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
    checkpoint: Option<CheckpointConfig<S>>,
) where
    S: WorkflowState + Clone + Send + Sync + Serialize + 'static,
    M: MergeStrategy<S>,
{
    let start_time = Instant::now();
    let mut execution_log: Vec<ExecutionEntry> = Vec::new();
    let mut engine = ExecutionEngine::new(state, None, cancel.clone());
    let mut current = graph.start_node().to_string();
    let mut step: usize = 0;
    // 缓存通配决策 — 一次发送，多次匹配
    let mut wildcard_cache = std::collections::HashMap::new();

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
                        error: GraphError::Terminal(crate::error::TerminalError::NodeNotFound(
                            current.clone(),
                        )),
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
                let mut ctx = engine.build_leaf_context();
                <ConditionNode<S> as LeafNode<S>>::execute(n, &mut ctx)
                    .await
                    .is_ok()
            }
            NodeKind::Barrier(n) => {
                let mut ctx = engine.build_leaf_context();
                <BarrierNode<S> as LeafNode<S>>::execute(n, &mut ctx)
                    .await
                    .is_ok()
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
                    error: GraphError::Terminal(crate::error::TerminalError::NodeExecutionFailed {
                        node: node_name,
                        source: "node execution failed".into(),
                    }),
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

                    let reroute_target =
                        apply_barrier_decision_generic(engine.state_mut(), &node_name, &d);

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
                        &BarrierDecision::Reject {
                            reason: "timeout".into(),
                        },
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
                send_complete(&event_tx, trace_id, engine, execution_log, start_time);
                break;
            }
            NextAction::Goto(target) => {
                current = target;
            }
            NextAction::Next => {
                if current == graph.end_node() {
                    send_complete(&event_tx, trace_id, engine, execution_log, start_time);
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

        // Checkpoint Save Path — 路由已解析，current 是下一个节点
        if let Some(ref cp_config) = checkpoint {
            let should_save = match cp_config.policy {
                CheckpointPolicy::EveryNode => true,
                CheckpointPolicy::BarrierOnly => false,
                CheckpointPolicy::Manual => false,
            };
            if should_save {
                let cp = Checkpoint::new(&current, engine.state().clone(), cp_config.graph_hash);
                if let Err(e) = (cp_config.save_fn)(cp, trace_id).await {
                    tracing::warn!(error = %e, "checkpoint save failed");
                }
            }
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
