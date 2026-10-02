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

use crate::checkpoint::{Checkpoint, CheckpointSink, TraceId};
use crate::error::{GraphError, TerminalError};
use crate::event::{BarrierDecisionMessage, BarrierId, GraphEvent};
use crate::exec::checkpoint_save_sink::CheckpointSaveSink;
use crate::exec::execution_engine::ExecutionEngine;
use crate::graph::{Graph, StepCallback};
use crate::ids::SpanId;
use crate::node::barrier_sink::ChannelBarrierSink;
use crate::state::workflow_state::WorkflowState;
use crate::state::{ExecutionEntry, GraphResult};

// ─── CheckpointConfig ──────────────────────────────────────────

/// Checkpoint 保存配置 — 传入 `run_execution_loop` 即可启用自动保存。
#[derive(Clone)]
pub struct CheckpointConfig<S: WorkflowState> {
    /// 保留策略
    pub retention: crate::checkpoint::checkpoint_policy::RetentionPolicy,
    /// 保存回调
    pub(crate) save_fn: Arc<crate::checkpoint::checkpoint_policy::CheckpointSaveFn<S>>,
    /// 图结构指纹
    pub(crate) graph_hash: u64,
    /// 存储后端引用（用于 prune）
    pub(crate) store: Option<Arc<dyn crate::checkpoint::store::BlobCheckpointStore>>,
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
            retention: crate::checkpoint::checkpoint_policy::RetentionPolicy::default(),
            graph_hash,
            store: None,
        }
    }

    pub fn with_retention(
        mut self,
        retention: crate::checkpoint::checkpoint_policy::RetentionPolicy,
    ) -> Self {
        self.retention = retention;
        self
    }

    pub fn with_store(
        mut self,
        store: Arc<dyn crate::checkpoint::store::BlobCheckpointStore>,
    ) -> Self {
        self.store = Some(store);
        self
    }

    /// 便捷构造器 — 从 store + codec 构建 save_fn（serialize + save_with_trace）。
    pub fn for_store(
        store: Arc<dyn crate::checkpoint::store::BlobCheckpointStore>,
        codec: impl crate::checkpoint::checkpoint_codec::CheckpointCodec<S>
        + Clone
        + Send
        + Sync
        + 'static,
        graph_hash: u64,
    ) -> Self
    where
        S: 'static,
    {
        let config_store = store.clone();
        let save_fn: crate::checkpoint::checkpoint_policy::CheckpointSaveFn<S> =
            Box::new(move |cp, trace_id| {
                let store = store.clone();
                let codec = codec.clone();
                Box::pin(async move {
                    let blob = codec.serialize(&cp, graph_hash)?;
                    store.save_with_trace(&trace_id, &blob).await
                })
            });
        Self {
            save_fn: Arc::new(save_fn),
            retention: crate::checkpoint::checkpoint_policy::RetentionPolicy::default(),
            graph_hash,
            store: Some(config_store),
        }
    }

    #[allow(clippy::collapsible_if)]
    pub async fn apply_retention(
        &self,
        trace_id: &TraceId,
    ) -> Result<(), crate::checkpoint::CheckpointStoreError> {
        if let Some(keep) = self.retention.prune_keep() {
            if let Some(store) = &self.store {
                let pruned = store.prune(trace_id, keep).await?;
                if pruned > 0 {
                    tracing::debug!(pruned, keep, "checkpoint pruned");
                }
            }
        }
        Ok(())
    }

    /// 恢复最新性校验 — 传入检查点必须是该 trace 的最新检查点。
    ///
    /// `store` 不存在时跳过（无持久化 → 无最新性概念）。
    /// 只接受该 trace 的最新检查点并续写原 trace，避免未定义的历史分叉。
    pub async fn check_restore_latest(
        &self,
        trace_id: &TraceId,
        cp: &Checkpoint<S>,
    ) -> Result<(), GraphError> {
        if let Some(store) = &self.store {
            match store.load_latest(trace_id).await {
                Ok(Some(latest)) => {
                    if latest.id != cp.checkpoint_id {
                        return Err(GraphError::Terminal(TerminalError::RestoreNotLatest {
                            checkpoint: cp.checkpoint_id.to_string(),
                            latest: latest.id.to_string(),
                        }));
                    }
                }
                Ok(None) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!("no checkpoints found for trace {trace_id}"),
                    }));
                }
                Err(e) => {
                    // 存储读取失败 → 存储故障语义（非"没有检查点"，非恢复校验失败）
                    return Err(GraphError::Terminal(TerminalError::CheckpointLoadFailed {
                        error: e.to_string(),
                    }));
                }
            }
        }
        Ok(())
    }

    /// 首次执行新鲜度校验 — 要求该 trace 无既有检查点。
    ///
    /// `store` 不存在时跳过（无持久化 → 无既有检查点）。
    /// 已有检查点 → `TraceNotFresh`（调用方应改用恢复入口）；
    /// 存储读取失败 → `CheckpointLoadFailed`（存储故障语义，非用户输入错误）。
    pub async fn assert_fresh_trace(&self, trace_id: &TraceId) -> Result<(), GraphError> {
        if let Some(store) = &self.store {
            match store.load_latest(trace_id).await {
                Ok(Some(latest)) => {
                    return Err(GraphError::Terminal(TerminalError::TraceNotFresh {
                        trace: trace_id.to_string(),
                        latest: latest.id.to_string(),
                    }));
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(GraphError::Terminal(TerminalError::CheckpointLoadFailed {
                        error: e.to_string(),
                    }));
                }
            }
        }
        Ok(())
    }
}

// ─── EventStepCallback ──────────────────────────────────────────

/// StepCallback 实现 — 用于 run_execution_loop 追踪执行日志 + 发射 per-node 事件。
struct EventStepCallback<S: WorkflowState> {
    start_time: Instant,
    execution_log: Vec<ExecutionEntry>,
    event_tx: Option<tokio::sync::mpsc::Sender<GraphEvent<S>>>,
    trace_id: TraceId,
}

impl<S: WorkflowState> EventStepCallback<S> {
    fn new(
        start_time: Instant,
        event_tx: tokio::sync::mpsc::Sender<GraphEvent<S>>,
        trace_id: TraceId,
    ) -> Self {
        Self {
            start_time,
            execution_log: Vec::new(),
            event_tx: Some(event_tx),
            trace_id,
        }
    }

    fn into_log(self) -> Vec<ExecutionEntry> {
        self.execution_log
    }
}

impl<S: WorkflowState + Send + 'static> StepCallback<'_> for EventStepCallback<S> {
    fn on_node_start(&mut self, node_name: &str, span_id: SpanId, step: usize) {
        if let Some(ref tx) = self.event_tx {
            let _ = tx.try_send(GraphEvent::NodeStart {
                node_name: node_name.to_string(),
                trace_id: self.trace_id,
                span_id,
                step,
            });
        }
    }

    fn on_node_end(
        &mut self,
        node_name: &str,
        span_id: SpanId,
        step: usize,
        duration: Duration,
        success: bool,
    ) {
        // 记录执行日志
        let node_end = self
            .start_time
            .checked_add(duration)
            .unwrap_or(self.start_time);
        self.execution_log.push(ExecutionEntry {
            step,
            node_name: node_name.to_string(),
            start_time: self.start_time,
            end_time: node_end,
            success,
            error: None,
        });

        // 发射 NodeEnd 事件
        if let Some(ref tx) = self.event_tx {
            let _ = tx.try_send(GraphEvent::NodeEnd {
                node_name: node_name.to_string(),
                trace_id: self.trace_id,
                span_id,
                success,
                duration,
            });
        }
    }

    fn on_barrier_waiting(&mut self, barrier_id: &BarrierId, node_name: &str, span_id: SpanId) {
        if let Some(ref tx) = self.event_tx {
            let _ = tx.try_send(GraphEvent::BarrierWaiting {
                barrier_id: barrier_id.clone(),
                node_name: node_name.to_string(),
                span_id,
            });
        }
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
#[allow(clippy::too_many_arguments)]
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
    _trace_sink: Option<crate::checkpoint::trace::MemoryTraceSink<S::Mutation>>,
    restore_from: Option<Checkpoint<S>>,
) where
    S: WorkflowState + Clone + Send + Sync + Serialize + 'static,
    S::Mutation: Clone + Send + Sync,
    M: crate::state::workflow_state::MergeStrategy<S>,
{
    let start_time = Instant::now();

    // 恢复路径：从 Checkpoint 恢复 State
    let restore_state = restore_from.as_ref().map(|cp| S::restore(cp.state.clone()));
    let mut engine_state = restore_state.unwrap_or(state);

    // 组装 Barrier Sink
    let mut barrier_sink = ChannelBarrierSink::new(decision_rx, cancel_rx, cancel.clone());

    // 组装 Checkpoint Sink（同步保存 + CheckpointSaved 事件）
    let mut cp_sink: Option<CheckpointSaveSink<S>> =
        checkpoint.map(|cfg| CheckpointSaveSink::new(cfg, trace_id, Some(event_tx.clone())));

    // 发射 GraphStart
    let _ = event_tx.send(GraphEvent::GraphStart { trace_id }).await;

    // 「恢复已完成」：零执行 — 发 GraphStart + GraphComplete，
    // 不执行任何节点、不保存新检查点（完成态 + 预算耗尽合法）
    if let Some(cp) = &restore_from {
        if cp.next_node.is_none() {
            let duration = start_time.elapsed();
            let result = GraphResult {
                trace_id,
                state: engine_state,
                execution_log: Vec::new(),
                duration,
                trace: None,
            };
            let _ = event_tx.try_send(GraphEvent::GraphComplete { result });
            return;
        }
    }

    // 恢复分支：从 next_node 续跑，预算从 steps_used 延续（max_steps 为总预算）
    let (start_node, steps_used) = match &restore_from {
        Some(cp) => (
            cp.next_node
                .as_ref()
                .expect("next_node 为 Some（上面已处理 None）")
                .0
                .clone(),
            cp.steps_used,
        ),
        None => (graph.start_node().to_string(), 0),
    };

    // step_cb 在 Engine 外部创建，以便在 Engine drop 后获取 execution_log
    let mut step_cb = EventStepCallback::new(start_time, event_tx.clone(), trace_id);

    // 在块作用域中创建 Engine，限制借用生命周期
    let result = {
        let mut engine = ExecutionEngine::new(
            &mut engine_state,
            None,
            cancel.clone(),
            cp_sink.as_mut().map(|s| s as &mut dyn CheckpointSink<S>),
            Some(&mut barrier_sink),
        );
        graph
            .run_inline_from(
                &mut engine,
                &start_node,
                steps_used,
                max_steps,
                &mut step_cb,
            )
            .await
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
    trace_sink: Option<crate::checkpoint::trace::MemoryTraceSink<S::Mutation>>,
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

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod restore_latestness_tests {
    use super::*;
    use crate::checkpoint::{
        BlobCheckpointStore, CHECKPOINT_FORMAT_VERSION, Checkpoint, CheckpointBlob,
        CheckpointCodec, CheckpointId, CheckpointStoreError, NodeId,
    };
    use crate::error::TerminalError;
    use crate::{
        GraphBuilder, InMemoryBlobStore, NodeKind, SerdeCheckpointCodec, State, StateMerge,
        TaskNode,
    };
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn single_node_graph() -> (crate::Graph, u64) {
        let mut b = GraphBuilder::<State, StateMerge>::new("t");
        b.start("a");
        b.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        b.end("a");
        let g = b.build().expect("build");
        let h = g.canonical_hash();
        (g, h)
    }

    fn cp(id: CheckpointId, hash: u64) -> Checkpoint<State> {
        Checkpoint {
            format_version: CHECKPOINT_FORMAT_VERSION,
            checkpoint_id: id,
            next_node: Some(NodeId("a".into())),
            state: State::new(),
            graph_hash: hash,
            steps_used: 0,
            created_at: SystemTime::now(),
        }
    }

    #[tokio::test]
    async fn check_restore_latest_accepts_latest() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let tid = crate::TraceId::new();
        let id = CheckpointId::new();
        let blob = codec.serialize(&cp(id, h), h).unwrap();
        store.save_with_trace(&tid, &blob).await.unwrap();

        let cfg = CheckpointConfig::for_store(store, codec, h);
        assert!(cfg.check_restore_latest(&tid, &cp(id, h)).await.is_ok());
    }

    #[tokio::test]
    async fn check_restore_latest_rejects_stale() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let tid = crate::TraceId::new();
        let stale_id = CheckpointId::new();
        let latest_id = CheckpointId::new();
        // 先存 stale，再存 latest（latest 成为 load_latest 结果）
        let b1 = codec.serialize(&cp(stale_id, h), h).unwrap();
        store.save_with_trace(&tid, &b1).await.unwrap();
        let b2 = codec.serialize(&cp(latest_id, h), h).unwrap();
        store.save_with_trace(&tid, &b2).await.unwrap();

        let cfg = CheckpointConfig::for_store(store, codec, h);
        let err = cfg
            .check_restore_latest(&tid, &cp(stale_id, h))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::RestoreNotLatest { .. })
        ));
    }

    #[tokio::test]
    async fn check_restore_latest_no_store_skips() {
        let (_g, h) = single_node_graph();
        let cfg = CheckpointConfig::new(
            |_c: Checkpoint<State>, _t: crate::TraceId| {
                Box::pin(async { Ok::<(), crate::checkpoint::CheckpointStoreError>(()) })
            },
            h,
        );
        // store=None → 跳过最新性校验
        assert!(
            cfg.check_restore_latest(&crate::TraceId::new(), &cp(CheckpointId::new(), h))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn assert_fresh_trace_rejects_existing() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let tid = crate::TraceId::new();
        let blob = codec.serialize(&cp(CheckpointId::new(), h), h).unwrap();
        store.save_with_trace(&tid, &blob).await.unwrap();

        let cfg = CheckpointConfig::for_store(store, codec, h);
        let err = cfg.assert_fresh_trace(&tid).await.unwrap_err();
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::TraceNotFresh { .. })
        ));
    }

    #[tokio::test]
    async fn assert_fresh_trace_accepts_empty() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(InMemoryBlobStore::new());
        let codec = SerdeCheckpointCodec::<State>::new();
        let cfg = CheckpointConfig::for_store(store, codec, h);
        assert!(cfg.assert_fresh_trace(&crate::TraceId::new()).await.is_ok());
    }

    /// load_latest 总是失败的 store — 验证存储故障语义（非"没有检查点"、非放行）。
    struct FailingStore;

    #[async_trait::async_trait]
    impl BlobCheckpointStore for FailingStore {
        async fn save_with_trace(
            &self,
            _trace_id: &crate::TraceId,
            _blob: &CheckpointBlob,
        ) -> Result<(), CheckpointStoreError> {
            Err(CheckpointStoreError::Storage("simulated failure".into()))
        }
        async fn load(
            &self,
            _id: &CheckpointId,
        ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
            Err(CheckpointStoreError::Storage("simulated failure".into()))
        }
        async fn load_latest(
            &self,
            _trace_id: &crate::TraceId,
        ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
            Err(CheckpointStoreError::Storage(
                "simulated read failure".into(),
            ))
        }
        async fn list(
            &self,
            _trace_id: &crate::TraceId,
        ) -> Result<Vec<CheckpointId>, CheckpointStoreError> {
            Err(CheckpointStoreError::Storage("simulated failure".into()))
        }
        async fn delete(&self, _id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
            Err(CheckpointStoreError::Storage("simulated failure".into()))
        }
        async fn prune(
            &self,
            _trace_id: &crate::TraceId,
            _keep: usize,
        ) -> Result<usize, CheckpointStoreError> {
            Err(CheckpointStoreError::Storage("simulated failure".into()))
        }
    }

    #[tokio::test]
    async fn check_restore_latest_load_failure_is_storage_error() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(FailingStore);
        let codec = SerdeCheckpointCodec::<State>::new();
        let cfg = CheckpointConfig::for_store(store, codec, h);
        let err = cfg
            .check_restore_latest(&crate::TraceId::new(), &cp(CheckpointId::new(), h))
            .await
            .unwrap_err();
        // 存储读取失败 → CheckpointLoadFailed（不当作"没有检查点"，不放行）
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::CheckpointLoadFailed { .. })
        ));
    }

    #[tokio::test]
    async fn assert_fresh_trace_load_failure_is_storage_error() {
        let (_g, h) = single_node_graph();
        let store = Arc::new(FailingStore);
        let codec = SerdeCheckpointCodec::<State>::new();
        let cfg = CheckpointConfig::for_store(store, codec, h);
        let err = cfg
            .assert_fresh_trace(&crate::TraceId::new())
            .await
            .unwrap_err();
        // 存储读取失败 → CheckpointLoadFailed（非 TraceNotFresh，不放行）
        assert!(matches!(
            err,
            GraphError::Terminal(TerminalError::CheckpointLoadFailed { .. })
        ));
    }
}
