//! CheckpointSaveSink — 包装 CheckpointConfig 为 CheckpointSink。
//!
//! 第一阶段语义（固定）：每个成功提交的节点都**同步等待**检查点保存，
//! 完成态也保存。保存失败 → 返回 Err → 执行在该边界停止。

use std::sync::Arc;

use async_trait::async_trait;

use crate::checkpoint::{
    Checkpoint, CheckpointSink, CheckpointStoreError, FrameInfo, RetentionPolicy, TraceId,
};
use crate::event::GraphEvent;
use crate::state::workflow_state::WorkflowState;

/// Checkpoint 保存 Sink — 同步等待保存完成，发射 CheckpointSaved 事件（尽力而为）。
pub struct CheckpointSaveSink<S: WorkflowState> {
    save_fn: Arc<crate::checkpoint::checkpoint_policy::CheckpointSaveFn<S>>,
    graph_hash: u64,
    trace_id: TraceId,
    retention: RetentionPolicy,
    store: Option<Arc<dyn crate::checkpoint::store::BlobCheckpointStore>>,
    event_tx: Option<tokio::sync::mpsc::Sender<GraphEvent<S>>>,
}

impl<S: WorkflowState> CheckpointSaveSink<S> {
    /// 从 CheckpointConfig 创建 Sink。
    ///
    /// `event_tx` — CheckpointSaved 事件通道（尽力而为观测，try_send 可能丢）。
    pub fn new(
        config: crate::exec::execution_loop::CheckpointConfig<S>,
        trace_id: TraceId,
        event_tx: Option<tokio::sync::mpsc::Sender<GraphEvent<S>>>,
    ) -> Self {
        Self {
            save_fn: config.save_fn,
            graph_hash: config.graph_hash,
            trace_id,
            retention: config.retention,
            store: config.store,
            event_tx,
        }
    }
}

#[async_trait]
impl<S: WorkflowState + 'static> CheckpointSink<S> for CheckpointSaveSink<S> {
    async fn on_checkpoint(
        &mut self,
        state: &S,
        frame: &FrameInfo,
    ) -> Result<(), CheckpointStoreError> {
        let cp = Checkpoint::new(frame.next_node.clone(), state, self.graph_hash, frame.step);
        let cp_id = cp.checkpoint_id;

        // ★ 同步等待保存 — 失败即返回 Err，不越过边界
        (self.save_fn)(cp, self.trace_id).await?;

        // CheckpointSaved 事件 — 尽力而为的观测（try_send 可能丢，不作可靠握手）
        if let Some(tx) = &self.event_tx {
            let _ = tx.try_send(GraphEvent::CheckpointSaved {
                checkpoint_id: cp_id,
                next_node: frame.next_node.as_ref().map(|n| n.0.clone()),
                step: frame.step,
            });
        }

        // 保留策略 prune — best-effort（检查点已落盘即视为越过边界）
        if let Some(keep) = self.retention.prune_keep() {
            if keep == 0 {
                return Err(CheckpointStoreError::Storage(
                    "KeepLatest(0) 无效：会删除全部恢复点".into(),
                ));
            }
            if let Some(s) = &self.store {
                if let Err(e) = s.prune(&self.trace_id, keep).await {
                    tracing::warn!(error = %e, "checkpoint prune failed");
                }
            }
        }
        Ok(())
    }
}
