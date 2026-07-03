//! OwnedExecutionEngine — 拥有 State 所有权的执行引擎。
//!
//! 用于 Parallel 分支等需要独立 State 副本的场景。
//! 与 [`ExecutionEngine`](crate::execution_engine::ExecutionEngine) 的区别：
//! - `ExecutionEngine<'a, S>` 借用 `&'a mut S`，用于主执行路径
//! - `OwnedExecutionEngine<S>` 拥有 `S`，用于需要独立 State 副本的场景

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::execution_engine::{
    ExecutionControl, ExecutionView, ExecutorState, NextAction, NodeMetadata,
};
use crate::node_context::{LeafContext, NodeContext};
use crate::stream_chunk::StreamChunk;
use crate::stream_emitter::StreamSink;
use crate::workflow_state::WorkflowState;

/// 拥有 State 所有权的执行引擎 — 用于 Parallel 分支等需要独立 State 的场景。
pub struct OwnedExecutionEngine<S: WorkflowState> {
    inner: S,
    stream: Option<Arc<dyn StreamSink>>,
    cancel: CancellationToken,
    control: ExecutionControl,
    metadata: NodeMetadata,
    mutations: Vec<S::Mutation>,
}

impl<S: WorkflowState> OwnedExecutionEngine<S> {
    /// 创建拥有 State 所有权的 Engine（用于 Parallel 分支等场景）。
    pub fn new(state: S, stream: Option<Arc<dyn StreamSink>>, cancel: CancellationToken) -> Self {
        Self {
            inner: state,
            stream,
            cancel,
            control: ExecutionControl::new(),
            metadata: NodeMetadata::default(),
            mutations: Vec::new(),
        }
    }

    /// 消费并返回最终状态。
    pub fn into_state(self) -> S {
        self.inner
    }

    pub fn state(&self) -> &S {
        &self.inner
    }

    pub fn state_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn stream_sink(&self) -> Option<Arc<dyn StreamSink>> {
        self.stream.clone()
    }

    pub fn commit(&mut self) {
        let batch = std::mem::take(&mut self.mutations);
        if !batch.is_empty() {
            self.inner.apply_batch(batch);
        }
    }
}

impl<S: WorkflowState> ExecutionView<S> for OwnedExecutionEngine<S> {
    fn state(&self) -> &S {
        &self.inner
    }

    fn emit(&self, chunk: StreamChunk) {
        if let Some(ref stream) = self.stream {
            stream.emit(chunk);
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

impl<S: WorkflowState> ExecutorState<S> for OwnedExecutionEngine<S> {
    fn build_node_context(&mut self) -> NodeContext<'_, S> {
        NodeContext {
            state: &mut self.inner,
            stream: self.stream.as_deref(),
            cancel: &self.cancel,
            control: &mut self.control,
            metadata: &mut self.metadata,
            mutations: &mut self.mutations,
        }
    }

    fn build_leaf_context(&mut self) -> LeafContext<'_, S> {
        LeafContext {
            state: &self.inner,
            stream: self.stream.as_deref(),
            cancel: &self.cancel,
            control: &mut self.control,
            metadata: &mut self.metadata,
            mutations: &mut self.mutations,
        }
    }

    fn clone_state(&self) -> S {
        self.inner.clone()
    }

    fn replace_state(&mut self, state: S) {
        self.inner = state;
    }

    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = S::Mutation>) {
        self.inner.apply_batch(mutations);
    }

    fn take_control(&mut self) -> (NextAction, Option<crate::ExecutionSignal>) {
        self.control.take()
    }

    fn take_metadata(&mut self) -> NodeMetadata {
        std::mem::take(&mut self.metadata)
    }
}
