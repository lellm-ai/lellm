//! Graph 流式执行循环 — Sink 组装层。
//!
//! 职责：组装 Sink（Barrier/Checkpoint），调用 `graph.run_inline()`，
//! 发射 `GraphEvent` 边界事件（GraphStart / GraphComplete / GraphError）。
//!
//! 执行逻辑统一由 `Graph::run_inline()` 负责，本模块不再包含执行循环。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::barrier_sink::ChannelBarrierSink;
use crate::checkpoint::{Checkpoint, CheckpointSink, FrameInfo, TraceId};
use crate::event::{BarrierDecisionMessage, GraphEvent};
use crate::execution_engine::ExecutionEngine;
use crate::graph::{Graph, StepCallback};
use crate::state::{ExecutionEntry, GraphResult};
use crate::workflow_state::WorkflowState;

// ─── CheckpointConfig ──────────────────────────────────────────

/// Checkpoint 保存配置 — 传入 `run_execution_loop` 即可启用自动保存。
pub struct CheckpointConfig<S: WorkflowState> {
    /// 触发策略
    pub trigger: crate::checkpoint_policy::TriggerPolicy,
    /// 保留策略
    pub retention: crate::checkpoint_policy::RetentionPolicy,
    /// 保存回调
    save_fn: Arc<crate::checkpoint_policy::CheckpointSaveFn<S>>,
    /// 图结构指纹
    graph_hash: u64,
    /// 存储后端引用（用于 prune）
    store: Option<Arc<dyn crate::store::BlobCheckpointStore>>,
}

impl<S: WorkflowState> CheckpointConfig<S> {
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
            save_fn: Arc::new(Box::new(save_fn)),
            trigger: crate::checkpoint_policy::TriggerPolicy::default(),
            retention: crate::checkpoint_policy::RetentionPolicy::default(),
            graph_hash,
            store: None,
        }
    }

    pub fn with_trigger(mut self, trigger: crate::checkpoint_policy::TriggerPolicy) -> Self {
        self.trigger = trigger;
        self
    }

    pub fn with_retention(mut self, retention: crate::checkpoint_policy::RetentionPolicy) -> Self {
        self.retention = retention;
        self
    }

    pub fn with_store(mut self, store: Arc<dyn crate::store::BlobCheckpointStore>) -> Self {
        self.store = Some(store);
        self
    }

    #[allow(deprecated)]
    pub fn with_policy(mut self, policy: crate::checkpoint::CheckpointPolicy) -> Self {
        self.trigger = policy.into();
        self
    }

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

// ─── CheckpointSaveSink ─────────────────────────────────────────

/// Checkpoint 保存 Sink — 包装 CheckpointConfig 为 CheckpointSink。
pub struct CheckpointSaveSink<S: WorkflowState> {
    save_fn: Arc<crate::checkpoint_policy::CheckpointSaveFn<S>>,
    graph_hash: u64,
    trace_id: TraceId,
    retention: crate::checkpoint_policy::RetentionPolicy,
    store: Option<Arc<dyn crate::store::BlobCheckpointStore>>,
}

impl<S: WorkflowState> CheckpointSaveSink<S> {
    pub fn new(config: CheckpointConfig<S>, trace_id: TraceId) -> Self {
        Self {
            save_fn: config.save_fn,
            graph_hash: config.graph_hash,
            trace_id,
            retention: config.retention,
            store: config.store,
        }
    }
}

impl<S: WorkflowState + 'static> CheckpointSink<S> for CheckpointSaveSink<S> {
    fn on_checkpoint(&mut self, state: &S, frame: &FrameInfo) {
        let save_fn = self.save_fn.clone();
        let graph_hash = self.graph_hash;
        let trace_id = self.trace_id;
        let retention = self.retention.clone();
        let store = self.store.clone();
        let cp = Checkpoint::new(frame.node_id.clone(), state, graph_hash);

        tokio::spawn(async move {
            match save_fn(cp, trace_id).await {
                Ok(()) => {
                    if let Some(keep) = retention.prune_keep() {
                        if let Some(ref s) = store {
                            if let Err(e) = s.prune(&trace_id, keep).await {
                                tracing::warn!(error = %e, "checkpoint retention failed");
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "checkpoint save failed");
                }
            }
        });
    }
}

// ─── EventStepCallback ──────────────────────────────────────────

/// StepCallback 实现 — 用于 run_execution_loop 追踪执行日志。
struct EventStepCallback {
    start_time: Instant,
    execution_log: Vec<ExecutionEntry>,
}

impl EventStepCallback {
    fn new(start_time: Instant) -> Self {
        Self {
            start_time,
            execution_log: Vec::new(),
        }
    }

    fn into_log(self) -> Vec<ExecutionEntry> {
        self.execution_log
    }
}

impl StepCallback<'_> for EventStepCallback {
    fn on_step(&mut self, node_name: &str, step: usize, duration: Duration) {
        let node_end = self
            .start_time
            .checked_add(duration)
            .unwrap_or(self.start_time);
        self.execution_log.push(ExecutionEntry {
            step,
            node_name: node_name.to_string(),
            start_time: self.start_time,
            end_time: node_end,
            success: true,
            error: None,
        });
    }
}

// ─── run_execution_loop ─────────────────────────────────────────

/// 运行 Graph 的流式执行循环。
///
/// 在 `tokio::spawn` 中调用，通过 channel 发射 `GraphEvent`。
///
/// # Sink 组装
///
/// ```text
/// run_execution_loop
///   ├── ChannelBarrierSink  — Barrier 等待 + 决策注入
///   ├── CheckpointSaveSink  — Checkpoint 保存（可选）
///   └── graph.run_inline()  — 唯一执行路径
/// ```
pub(crate) async fn run_execution_loop<S, M>(
    graph: Arc<Graph<S, M>>,
    state: S,
    max_steps: usize,
    trace_id: TraceId,
    event_tx: tokio::sync::mpsc::Sender<GraphEvent<S>>,
    decision_rx: tokio::sync::mpsc::Receiver<BarrierDecisionMessage>,
    cancel_rx: tokio::sync::mpsc::Receiver<()>,
    cancel: CancellationToken,
    checkpoint: Option<CheckpointConfig<S>>,
    _trace_sink: Option<crate::trace::MemoryTraceSink<S::Mutation>>,
    restore_from: Option<Checkpoint<S>>,
) where
    S: WorkflowState + Clone + Send + Sync + Serialize + 'static,
    S::Mutation: Clone + Send + Sync,
    M: crate::workflow_state::MergeStrategy<S>,
{
    let start_time = Instant::now();

    // 恢复路径：从 Checkpoint 恢复 State
    let restore_state = restore_from.as_ref().map(|cp| S::restore(cp.state.clone()));
    let mut engine_state = restore_state.unwrap_or(state);

    // 组装 Barrier Sink
    let mut barrier_sink = ChannelBarrierSink::new(decision_rx, cancel_rx, cancel.clone());

    // 组装 Checkpoint Sink
    let mut cp_sink: Option<CheckpointSaveSink<S>> =
        checkpoint.map(|cfg| CheckpointSaveSink::new(cfg, trace_id));

    // 发射 GraphStart
    let _ = event_tx.send(GraphEvent::GraphStart { trace_id }).await;

    // step_cb 在 Engine 外部创建，以便在 Engine drop 后获取 execution_log
    let mut step_cb = EventStepCallback::new(start_time);

    // 在块作用域中创建 Engine，限制借用生命周期
    let result = {
        let mut engine = ExecutionEngine::new(
            &mut engine_state,
            None,
            cancel.clone(),
            cp_sink.as_mut().map(|s| s as &mut dyn CheckpointSink<S>),
            Some(&mut barrier_sink),
        );
        graph.run_inline(&mut engine, max_steps, &mut step_cb).await
    };

    // engine 已 drop，可以安全访问 engine_state
    let final_state = engine_state;
    let execution_log = step_cb.into_log();

    match result {
        Ok(()) => {
            let duration = start_time.elapsed();
            let graph_result = GraphResult {
                trace_id,
                state: final_state,
                execution_log,
                duration,
                trace: None,
            };
            let _ = event_tx.try_send(GraphEvent::GraphComplete {
                result: graph_result,
            });
        }
        Err(error) => {
            let _ = event_tx
                .send(GraphEvent::GraphError {
                    error,
                    state: final_state,
                })
                .await;
        }
    }
}

// ─── send_complete (deprecated) ─────────────────────────────────

/// 发送 GraphComplete 事件。
///
/// @deprecated — 由 run_execution_loop 内部处理。
#[allow(dead_code)]
pub(crate) fn send_complete<S: WorkflowState>(
    event_tx: &tokio::sync::mpsc::Sender<GraphEvent<S>>,
    trace_id: TraceId,
    final_state: &S,
    execution_log: Vec<ExecutionEntry>,
    start_time: Instant,
    trace_sink: Option<crate::trace::MemoryTraceSink<S::Mutation>>,
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
