//! Graph 层流式事件。
//!
//! P2: 流式事件穿透 — Graph 执行过程中实时发射事件，
//! 消费者可通过 channel 观察节点执行与 Agent 内部状态。
//!
//! BarrierPaused 事件携带 oneshot sender，消费者决策后发送 BarrierDecision。

use std::time::Duration;

use crate::error::GraphError;
use crate::state::GraphResult;

/// Barrier 审批决策。
///
/// 消费者收到 `GraphEvent::BarrierPaused` 后，通过 `signal` oneshot
/// 发送此决策，BarrierNode 根据决策继续执行。
#[derive(Debug, Clone)]
pub enum BarrierDecision {
    /// 通过 — 节点继续执行下一步
    Approve,
    /// 拒绝 — 写入拒绝原因到 State，由 edge_if 决定是否回跳
    Reject { reason: String },
    /// 修改 State 中的指定 key，然后继续
    Modify {
        key: String,
        value: serde_json::Value,
    },
    /// 跳转到指定节点（覆盖默认流转）
    Reroute { target: String },
}

/// Graph 层流式事件 — 封闭、强类型、exhaustive match。
///
/// 事件流生命周期：
/// - 正常结束：`GraphComplete` 恰好一次，然后 channel 关闭
/// - 异常结束：`GraphError` 恰好一次，然后 channel 关闭
/// - 终态事件后不再发送任何事件
#[derive(Debug)]
pub enum GraphEvent {
    /// 节点开始执行
    NodeStart {
        /// 节点名称
        node_name: String,
    },
    /// 节点执行完成
    NodeEnd {
        /// 节点名称
        node_name: String,
        /// 是否成功
        success: bool,
        /// 执行耗时
        duration: Duration,
    },
    /// Agent 节点内部事件（来自 ToolUseLoop）
    Agent {
        /// AgentNode 名称
        node_name: String,
        /// Agent 层事件
        event: lellm_agent::AgentEvent,
    },
    /// Barrier 暂停 — 等待外部审批信号。
    ///
    /// 消费者收到此事件后，通过 `signal` 发送 [`BarrierDecision`]。
    /// 如果消费者不处理，BarrierNode 将等待超时（如果配置了超时）。
    ///
    /// ⚠️ **必须处理** — 如果不发送决策，Graph 执行将永久阻塞。
    BarrierPaused {
        /// BarrierNode 名称
        node_name: String,
        /// 决策 oneshot sender — 消费者决策后发送
        signal: tokio::sync::oneshot::Sender<BarrierDecision>,
    },
    /// Graph 执行完成（恰好一次）
    GraphComplete {
        /// 执行结果
        result: GraphResult,
    },
    /// Graph 执行出错（恰好一次）
    GraphError {
        /// 错误信息
        error: GraphError,
    },
}

/// Graph 事件通道类型别名
pub type GraphStream = tokio::sync::mpsc::Receiver<GraphEvent>;
