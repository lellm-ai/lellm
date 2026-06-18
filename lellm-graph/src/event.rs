//! Graph 层流式事件。
//!
//! 事件分层设计：
//! - `GraphEvent` — 图级事件（节点边界、Barrier、完成、错误）
//! - `FlowEvent` — 节点内部事件中间层（解耦，不依赖任何具体节点类型）

use std::time::Duration;

use crate::checkpoint::CheckpointId;
use crate::error::{GraphError, ObservedError};
use crate::ids::{SpanId, TraceId};
use crate::state::GraphResult;

// ─── FlowEvent ───────────────────────────────────────────────

/// 节点内部事件 — 解耦的通用事件中间层。
#[derive(Debug)]
pub enum FlowEvent {
    /// 节点开始执行
    NodeStarted { node_id: String, span_id: SpanId },
    /// 节点执行完成
    NodeCompleted {
        node_id: String,
        span_id: SpanId,
        duration: Duration,
    },
    /// 节点执行失败
    NodeFailed { node_id: String, error: String },
    /// 状态变更
    StateChanged {
        node_id: String,
        delta: crate::delta::StateDelta,
    },
    /// 并行节点开始执行
    ParallelStarted {
        node_id: String,
        branch_count: usize,
        span_id: SpanId,
    },
    /// 并行节点执行完成
    ParallelCompleted {
        node_id: String,
        span_id: SpanId,
        duration: Duration,
    },
    /// 并行分支执行完成
    BranchCompleted {
        branch_name: String,
        node_id: String,
        span_id: SpanId,
        success: bool,
        duration: Duration,
    },
    /// 自定义事件 — 具体节点类型通过此变体注入内部事件。
    Custom {
        node_id: String,
        payload: Box<dyn std::any::Any + Send + Sync>,
    },
}

// ─── BarrierId / BarrierDecision ─────────────────────────────

/// Barrier 审批请求的唯一标识。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BarrierId {
    pub node_id: String,
    pub occurrence: u32,
}

impl BarrierId {
    pub fn new(node_id: impl Into<String>, occurrence: u32) -> Self {
        Self {
            node_id: node_id.into(),
            occurrence,
        }
    }
}

/// Barrier 审批决策。
#[derive(Debug, Clone)]
pub enum BarrierDecision {
    /// 通过
    Approve,
    /// 拒绝
    Reject { reason: String },
    /// 修改 State 中的指定 key，然后继续
    Modify {
        key: String,
        value: serde_json::Value,
    },
    /// 跳转到指定节点
    Reroute { target: String },
}

// ─── GraphEvent ───────────────────────────────────────────────

/// Graph 层流式事件。
#[derive(Debug)]
pub enum GraphEvent {
    /// Graph 执行开始（恰好一次）
    GraphStart { trace_id: TraceId },
    /// 节点开始执行
    NodeStart {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        step: usize,
    },
    /// 节点执行完成
    NodeEnd {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        success: bool,
        duration: Duration,
    },
    /// 节点内部事件（通过 FlowEvent 中间层）
    Node {
        span_id: SpanId,
        node_name: String,
        event: FlowEvent,
    },
    /// Barrier 暂停 — 等待外部审批信号。
    BarrierWaiting {
        barrier_id: BarrierId,
        node_name: String,
        span_id: SpanId,
    },
    /// Barrier 决策已应用
    BarrierResolved {
        barrier_id: BarrierId,
        decision: BarrierDecision,
    },
    /// 观测错误 — 不影响 control flow
    ObservedError {
        error: ObservedError,
        node_name: String,
    },
    /// Checkpoint 已保存。
    CheckpointSaved {
        checkpoint_id: CheckpointId,
        node_name: String,
        step: usize,
    },
    /// Graph 执行完成（恰好一次）
    GraphComplete { result: GraphResult },
    /// Graph 执行出错（恰好一次）
    GraphError { error: GraphError, state: crate::state::State },
}

/// Graph 事件通道类型别名
pub type GraphStream = tokio::sync::mpsc::Receiver<GraphEvent>;

/// Graph 流式执行的完整返回包装。
pub struct GraphExecution {
    pub stream: GraphStream,
    pub handle: GraphHandle,
}

// ─── GraphHandle ──────────────────────────────────────────────

/// Graph 执行句柄 — 用于与运行中的 Graph 交互。
pub struct GraphHandle {
    decision_tx: tokio::sync::mpsc::Sender<BarrierDecisionMessage>,
    cancel_tx: tokio::sync::mpsc::Sender<()>,
    checkpoint_tx: tokio::sync::mpsc::Sender<()>,
}

/// 决策消息 — 支持精确匹配和通配匹配。
#[allow(dead_code)]
pub(crate) enum BarrierDecisionMessage {
    Exact {
        barrier_id: BarrierId,
        decision: BarrierDecision,
    },
    Wildcard {
        node_id: String,
        decision: BarrierDecision,
    },
}

impl GraphHandle {
    pub(crate) fn new(
        decision_tx: tokio::sync::mpsc::Sender<BarrierDecisionMessage>,
        cancel_tx: tokio::sync::mpsc::Sender<()>,
        checkpoint_tx: tokio::sync::mpsc::Sender<()>,
    ) -> Self {
        Self {
            decision_tx,
            cancel_tx,
            checkpoint_tx,
        }
    }

    pub async fn decide(
        &self,
        barrier_id: BarrierId,
        decision: BarrierDecision,
    ) -> Result<(), GraphError> {
        self.decision_tx
            .send(BarrierDecisionMessage::Exact {
                barrier_id,
                decision,
            })
            .await
            .map_err(|_| {
                GraphError::Terminal(crate::error::TerminalError::BarrierCancelled {
                    node: "decision channel closed".into(),
                })
            })
    }

    pub async fn decide_wildcard(
        &self,
        node_id: impl Into<String>,
        decision: BarrierDecision,
    ) -> Result<(), GraphError> {
        self.decision_tx
            .send(BarrierDecisionMessage::Wildcard {
                node_id: node_id.into(),
                decision,
            })
            .await
            .map_err(|_| {
                GraphError::Terminal(crate::error::TerminalError::BarrierCancelled {
                    node: "decision channel closed".into(),
                })
            })
    }

    pub fn cancel(&self) {
        let _ = self.cancel_tx.try_send(());
    }

    pub async fn checkpoint(&self) -> Result<(), GraphError> {
        self.checkpoint_tx.send(()).await.map_err(|_| {
            GraphError::Terminal(crate::error::TerminalError::BarrierCancelled {
                node: "checkpoint channel closed".into(),
            })
        })
    }
}
