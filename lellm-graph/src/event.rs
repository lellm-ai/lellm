//! Graph 层流式事件。
//!
//! 事件分层设计：
//! - `GraphEvent` — 图级事件（节点边界、Barrier、完成、错误）
//! - `NodeEvent` — 节点内部事件中间层（隔离 Graph 与节点内部事件）
//!
//! `trace_id` 区分同一节点的不同执行实例（生命周期追踪）。
//!
//! Barrier 决策通过 `GraphHandle::decide()` 提交，不暴露 oneshot Sender。

use std::time::Duration;

use uuid::Uuid;

use crate::error::GraphError;
use crate::state::GraphResult;

// ─── TraceId ────────────────────────────────────────────────────

/// 节点执行实例的唯一标识。
///
/// 同一节点可能被多次执行（回跳循环），`trace_id` 区分"哪次运行"。
/// 日志天然可聚合：按 trace_id 过滤即可获取单次执行的所有事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId(Uuid);

impl TraceId {
    /// 生成新的 trace_id。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 获取 UUID 字符串表示。
    pub fn to_string(&self) -> String {
        self.0.to_string()
    }
}

// ─── BarrierId ──────────────────────────────────────────────────

/// Barrier 审批请求的唯一标识。
///
/// 消费者收到 `GraphEvent::BarrierPaused` 后，通过 `GraphHandle::decide(barrier_id, ...)`
/// 提交决策。BarrierId 屏蔽了内部同步原语（oneshot Sender），支持事件序列化与 Remote UI。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BarrierId(Uuid);

impl BarrierId {
    /// 生成新的 barrier_id。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

// ─── NodeEvent ──────────────────────────────────────────────────

/// 节点内部事件 — 隔离 Graph 与节点内部事件的中间层。
///
/// Graph 编排 Agent，不暴露 Agent 内部实现。`NodeEvent` 作为中间层，
/// 允许未来新增节点类型事件而不破坏 `GraphEvent` 的兼容性。
#[derive(Debug)]
pub enum NodeEvent {
    /// Agent 节点内部事件（来自 ToolUseLoop）
    Agent(lellm_agent::AgentEvent),
    /// Barrier 节点内部事件（预留）
    Barrier(BarrierInnerEvent),
}

/// Barrier 节点内部事件（预留扩展）。
#[derive(Debug, Clone)]
pub enum BarrierInnerEvent {
    /// Barrier 状态变更（预留）
    StateChange { from: String, to: String },
}

// ─── BarrierDecision ────────────────────────────────────────────

/// Barrier 审批决策。
///
/// 消费者收到 `GraphEvent::BarrierPaused` 后，通过 `GraphHandle::decide()`
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

// ─── GraphEvent ─────────────────────────────────────────────────

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
        /// 执行实例 ID
        trace_id: TraceId,
    },
    /// 节点执行完成
    NodeEnd {
        /// 节点名称
        node_name: String,
        /// 执行实例 ID
        trace_id: TraceId,
        /// 是否成功
        success: bool,
        /// 执行耗时
        duration: Duration,
    },
    /// 节点内部事件（通过 NodeEvent 中间层）
    Node {
        /// 执行实例 ID
        trace_id: TraceId,
        /// 节点名称
        node_name: String,
        /// 节点内部事件
        event: NodeEvent,
    },
    /// Barrier 暂停 — 等待外部审批信号。
    ///
    /// 消费者收到此事件后，通过 `GraphHandle::decide(barrier_id, decision)` 提交决策。
    /// 如果消费者不处理，BarrierNode 将等待超时（如果配置了超时）。
    ///
    /// ⚠️ **必须处理** — 如果不发送决策，Graph 执行将永久阻塞。
    BarrierPaused {
        /// Barrier 审批请求 ID（用于 GraphHandle::decide）
        barrier_id: BarrierId,
        /// BarrierNode 名称
        node_name: String,
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

// ─── GraphHandle ────────────────────────────────────────────────

/// Graph 执行句柄 — 用于与运行中的 Graph 交互。
///
/// 通过 `execute_stream()` 返回，消费者使用此句柄提交 Barrier 决策。
///
/// ```rust,ignore
/// let (mut stream, handle) = executor.execute_stream(graph, state);
/// while let Some(event) = stream.recv().await {
///     match event {
///         GraphEvent::BarrierPaused { barrier_id, node_name } => {
///             let decision = ask_user(&node_name).await;
///             handle.decide(barrier_id, decision).await?;
///         }
///         GraphEvent::GraphComplete { result } => { /* ... */ }
///         _ => {}
///     }
/// }
/// ```
pub struct GraphHandle {
    /// 决策 channel — 向执行循环提交 Barrier 决策
    decision_tx: tokio::sync::mpsc::Sender<(BarrierId, BarrierDecision)>,
}

impl GraphHandle {
    /// 创建 GraphHandle（内部使用）。
    pub fn new(decision_tx: tokio::sync::mpsc::Sender<(BarrierId, BarrierDecision)>) -> Self {
        Self { decision_tx }
    }

    /// 提交 Barrier 决策。
    ///
    /// - `barrier_id` — 来自 `GraphEvent::BarrierPaused` 的 ID
    /// - `decision` — 审批决策
    ///
    /// **一次性语义：** 每个 BarrierId 只能提交一次决策，重复提交返回错误。
    pub async fn decide(
        &self,
        barrier_id: BarrierId,
        decision: BarrierDecision,
    ) -> Result<(), GraphError> {
        self.decision_tx
            .send((barrier_id, decision))
            .await
            .map_err(|_| GraphError::BarrierCancelled {
                node: "decision channel closed".into(),
            })
    }
}
