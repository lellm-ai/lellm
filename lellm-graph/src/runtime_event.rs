//! RuntimeEvent — Control Plane 控制面事件。
//!
//! 低频、生命周期事件、拓扑事件、状态变化事件。
//! 与 StreamChunk（Data Plane）分离，避免高频数据事件撑爆控制事件通道。

use std::time::Duration;

use crate::checkpoint::CheckpointId;
use crate::ids::{SpanId, TraceId};

// ─── RuntimeEvent ─────────────────────────────────────────────

/// 控制面事件 — 低频、生命周期事件。
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// 执行开始
    ExecutionStarted {
        trace_id: TraceId,
        graph_name: String,
    },
    /// 节点开始执行
    NodeStarted {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        step: usize,
    },
    /// 节点执行完成
    NodeCompleted {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        duration: Duration,
    },
    /// 节点执行失败
    NodeFailed {
        node_name: String,
        trace_id: TraceId,
        span_id: SpanId,
        error: String,
    },
    /// 分支开始执行（并行节点）
    BranchStarted {
        node_name: String,
        branch_name: String,
        span_id: SpanId,
    },
    /// 分支执行完成（并行节点）
    BranchCompleted {
        node_name: String,
        branch_name: String,
        span_id: SpanId,
        success: bool,
        duration: Duration,
    },
    /// Barrier 等待外部决策
    BarrierWaiting {
        barrier_id: crate::event::BarrierId,
        node_name: String,
        span_id: SpanId,
    },
    /// Barrier 决策已应用
    BarrierResolved { barrier_id: crate::event::BarrierId },
    /// Checkpoint 已保存
    CheckpointCreated {
        checkpoint_id: CheckpointId,
        node_name: String,
        step: usize,
    },
    /// 执行完成
    ExecutionCompleted {
        trace_id: TraceId,
        duration: Duration,
    },
}
