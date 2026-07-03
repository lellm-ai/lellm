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
use crate::checkpoint::{Checkpoint, NodeId, TraceId};
use crate::checkpoint_policy::{RetentionPolicy, TriggerPolicy};
use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierDecisionMessage, GraphEvent};
use crate::execution_engine::{ExecutionEngine, ExecutionSignal, ExecutorState, NextAction};
use crate::graph::Graph;
use crate::ids::SpanId;
use crate::node::{BarrierNode, ConditionNode, FlowNode, LeafNode, NodeKind};
use crate::state::{ExecutionEntry, GraphResult};
use crate::trace::{MemoryTraceSink, TraceSink, TraceStep};
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

/// Checkpoint 保存配置 — 三层策略。
///
/// ```text
/// CheckpointConfig
///   ├── trigger:    何时保存（EveryNode / BarrierOnly / Manual / OnMutation）
///   ├── retention:  保留多少个（KeepAll / KeepLatest(N) / TimeBased）
///   ├── save_fn:    保存回调
///   └── store:      存储后端引用（用于 prune）
/// ```
pub struct CheckpointConfig<S: WorkflowState> {
    /// 触发策略
    pub trigger: TriggerPolicy,
    /// 保留策略
    pub retention: RetentionPolicy,
    /// 保存回调
    save_fn: CheckpointSaveFn<S>,
    /// 图结构指纹
    graph_hash: u64,
    /// 存储后端引用（用于 prune）
    store: Option<std::sync::Arc<dyn crate::store::BlobCheckpointStore>>,
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
            trigger: TriggerPolicy::default(),
            retention: RetentionPolicy::default(),
            graph_hash,
            store: None,
        }
    }

    /// 设置触发策略。
    pub fn with_trigger(mut self, trigger: TriggerPolicy) -> Self {
        self.trigger = trigger;
        self
    }

    /// 设置保留策略。
    pub fn with_retention(mut self, retention: RetentionPolicy) -> Self {
        self.retention = retention;
        self
    }

    /// 设置存储后端引用（用于 prune）。
    pub fn with_store(
        mut self,
        store: std::sync::Arc<dyn crate::store::BlobCheckpointStore>,
    ) -> Self {
        self.store = Some(store);
        self
    }

    /// 向后兼容 — 设置旧的 CheckpointPolicy（自动转换为 TriggerPolicy）。
    #[allow(deprecated)]
    pub fn with_policy(mut self, policy: crate::checkpoint::CheckpointPolicy) -> Self {
        self.trigger = policy.into();
        self
    }

    /// 根据策略判断是否应该保存。
    pub fn should_save(&self, has_mutations: bool, is_barrier: bool) -> bool {
        match self.trigger {
            TriggerPolicy::EveryNode => true,
            TriggerPolicy::BarrierOnly => is_barrier,
            TriggerPolicy::Manual => false,
            TriggerPolicy::OnMutation => has_mutations,
        }
    }

    /// 执行保留策略（prune）。
    pub async fn apply_retention(
        &self,
        trace_id: &TraceId,
    ) -> Result<(), crate::checkpoint::CheckpointStoreError> {
        if let Some(keep) = self.retention.prune_keep() {
            if let Some(ref store) = self.store {
                let pruned = store.prune(trace_id, keep).await?;
                if pruned > 0 {
                    tracing::debug!(pruned, keep, "checkpoint pruned");
                }
            }
        }
        Ok(())
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
///
/// # 恢复路径
///
/// `restore_from` 包含 Checkpoint 时：
/// 1. 使用 Checkpoint 中的 state 和 current_node
/// 2. 如果 current_node 是 Barrier → 立即 Re-Wait（等待新决策）
/// 3. decision 属于 Control Plane，不回放旧决策
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
    trace_sink: Option<MemoryTraceSink<S::Mutation>>,
    restore_from: Option<Checkpoint<S>>,
) where
    S: WorkflowState + Clone + Send + Sync + Serialize + 'static,
    S::Mutation: Clone + Send + Sync,
    M: MergeStrategy<S>,
{
    let start_time = Instant::now();
    let mut execution_log: Vec<ExecutionEntry> = Vec::new();

    // 恢复路径：使用 Checkpoint 中的 state 和 current_node
    // P0-1: checkpoint.state 是 S::Checkpoint，需要通过 restore() 转换为 S
    let restore_state = restore_from.as_ref().map(|cp| S::restore(cp.state.clone()));
    let mut engine_state = restore_state.unwrap_or(state);
    // execution_loop 是内部测试工具，不需要自动 checkpoint
    let mut engine = ExecutionEngine::new(&mut engine_state, None, cancel.clone(), None, None);
    let mut current = if let Some(ref cp) = restore_from {
        cp.current_node.0.clone()
    } else {
        graph.start_node().to_string()
    };
    let mut step: usize = 0;
    // 缓存通配决策 — 一次发送，多次匹配
    let mut wildcard_cache = std::collections::HashMap::new();
    // TraceSink — 审计日志
    let mut trace_sink = trace_sink;

    let _ = event_tx.send(GraphEvent::GraphStart { trace_id }).await;

    // Barrier Re-Wait 恢复路径
    // 如果从 Checkpoint 恢复且当前节点是 Barrier，立即重新等待决策。
    // Decision 属于 Control Plane，不回放旧决策。
    let mut skip_first_execution = false;
    if restore_from.is_some() {
        if let Some(node) = graph.nodes.get(&current) {
            if matches!(node, NodeKind::Barrier(_)) {
                let span_id = SpanId::new();
                let barrier_id = crate::event::BarrierId::new(&current, 0);

                let _ = event_tx
                    .send(GraphEvent::BarrierWaiting {
                        barrier_id: barrier_id.clone(),
                        node_name: current.clone(),
                        span_id,
                    })
                    .await;

                // 提取 Barrier 的 timeout（如果有）
                let barrier_timeout = if let NodeKind::Barrier(bn) = node {
                    bn.timeout
                } else {
                    None
                };

                let outcome = wait_for_barrier_decision(
                    &mut decision_rx,
                    &mut cancel_rx,
                    &cancel,
                    &barrier_id,
                    barrier_timeout,
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
                            apply_barrier_decision_generic(engine.state_mut(), &current, &d);

                        if let Some(target) = reroute_target {
                            current = target;
                        }
                        // 决策已应用，跳过 Barrier 节点的执行（只负责 pause）
                        skip_first_execution = true;
                    }
                    BarrierOutcome::TimedOut => {
                        let _ = event_tx
                            .send(GraphEvent::BarrierResolved {
                                barrier_id,
                                decision: BarrierDecision::Reject {
                                    reason: "timeout on restore".into(),
                                },
                            })
                            .await;
                        apply_barrier_decision_generic(
                            engine.state_mut(),
                            &current,
                            &BarrierDecision::Reject {
                                reason: "timeout on restore".into(),
                            },
                        );
                        skip_first_execution = true;
                    }
                    BarrierOutcome::Cancelled => {
                        let _ = event_tx
                            .send(GraphEvent::GraphError {
                                error: GraphError::Terminal(
                                    crate::error::TerminalError::BarrierCancelled {
                                        node: current.clone(),
                                    },
                                ),
                                state: engine.state().clone(),
                            })
                            .await;
                        return;
                    }
                }
            }
        }
    }

    loop {
        // 如果跳过了第一次执行（Barrier Re-Wait），直接进入路由解析
        if step == 0 && skip_first_execution {
            // Re-Wait 完成后，直接解析下一步（不执行 Barrier 节点本身）
            match graph.resolve_next_inline(&current, engine.state()) {
                Ok(target) => current = target,
                Err(_) => {
                    // 没有下一节点，检查是否到达终点
                    if current == graph.end_node() {
                        send_complete(
                            &event_tx,
                            trace_id,
                            engine.state(),
                            execution_log,
                            start_time,
                            trace_sink.take(),
                        );
                        break;
                    }
                    // 否则继续执行当前节点之后的流程
                    // （实际上不会走到这里，因为 resolve_next 应该成功）
                }
            }
            step += 1;
            continue;
        }
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
        let is_barrier = matches!(node, NodeKind::Barrier(_));
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
            NodeKind::Subgraph(subgraph) => {
                // Subgraph 执行 — 通过 StateProjector 投影状态 + 递归执行内层 Graph
                let stream = engine.stream_sink();
                let cancel = engine.cancel_token().clone();
                subgraph
                    .execute(engine.state_mut(), stream, cancel)
                    .await
                    .is_ok()
            }
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

        // commit mutations (Unit of Work) — 三段式流水线
        // P1: take batch → P3: trace/mutation-log → apply to state
        let commit_batch = engine.take_commit_batch();
        let has_mutations = !commit_batch.is_empty();

        // TraceSink 记录（如果启用）
        if let Some(ref mut sink) = trace_sink {
            if !commit_batch.is_empty() {
                sink.record_step(TraceStep {
                    step,
                    node_id: NodeId(node_name.clone()),
                    mutations: commit_batch.clone(),
                });
            }
        }

        engine.apply_batch_to_state(commit_batch);

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
                send_complete(
                    &event_tx,
                    trace_id,
                    engine.state(),
                    execution_log,
                    start_time,
                    trace_sink.take(),
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
                        engine.state(),
                        execution_log,
                        start_time,
                        trace_sink.take(),
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

        // Checkpoint Save Path — 路由已解析，current 是下一个节点
        if let Some(ref cp_config) = checkpoint {
            if cp_config.should_save(has_mutations, is_barrier) {
                // P0-1: Checkpoint::new 现在接受 &S，内部调用 snapshot() 进行投影
                let cp = Checkpoint::new(&current, engine.state(), cp_config.graph_hash);
                match (cp_config.save_fn)(cp, trace_id).await {
                    Ok(()) => {
                        // 保存成功后，应用保留策略
                        if let Err(e) = cp_config.apply_retention(&trace_id).await {
                            tracing::warn!(error = %e, "checkpoint retention failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "checkpoint save failed");
                    }
                }
            }
        }
    }
}

/// 发送 GraphComplete 事件。
pub(crate) fn send_complete<S: WorkflowState>(
    event_tx: &tokio::sync::mpsc::Sender<GraphEvent<S>>,
    trace_id: TraceId,
    final_state: &S,
    execution_log: Vec<ExecutionEntry>,
    start_time: Instant,
    trace_sink: Option<MemoryTraceSink<S::Mutation>>,
) {
    let duration = start_time.elapsed();
    let trace = trace_sink.map(|sink| sink.into_trace());
    let result = GraphResult {
        trace_id,
        state: final_state.clone(),
        execution_log,
        duration,
        trace,
    };
    let _ = event_tx.try_send(GraphEvent::GraphComplete { result });
}
