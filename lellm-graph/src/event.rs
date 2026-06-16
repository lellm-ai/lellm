//! Graph 层流式事件。
//!
//! 事件分层设计：
//! - `GraphEvent` — 图级事件（节点边界、Barrier、完成、错误）
//! - `NodeEvent` — 节点内部事件中间层
//!
//! 通过 `EventLevel` 支持 consumer 按级别 filter。
//! `TraceId` / `SpanId` 对标 tracing crate 的 trace/span 语义。

use std::time::Duration;

use uuid::Uuid;

use crate::error::{GraphError, ObservedError};
use crate::state::{GraphResult, State};

// ─── TraceId / SpanId ─────────────────────────────────────────

/// 一次 Graph Execution 的唯一标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId(Uuid);

impl TraceId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn to_string(&self) -> String {
        self.0.to_string()
    }
}

/// 一次 Node Execution 的唯一标识。
///
/// 同一节点可能被多次执行（回跳循环），每次进入生成新 SpanId。
/// TraceId → SpanId 形成树状结构，便于分层查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId(Uuid);

impl SpanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn to_string(&self) -> String {
        self.0.to_string()
    }
}

// ─── BarrierId ────────────────────────────────────────────────

/// Barrier 审批请求的唯一标识。
///
/// 由 `(node_id, occurrence)` 组成，支持通配决策。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BarrierId {
    /// 用户定义的 Barrier 节点名（可预测）
    pub node_id: String,
    /// 第几次到达（1-based）
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

// ─── EventLevel ───────────────────────────────────────────────

/// 事件级别 — 给 consumer 用的 filter hint。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventLevel {
    /// 图级事件（生命周期、错误）
    Graph,
    /// 节点级事件（边界、内部）
    Node,
    /// Agent 内部事件（ReAct 轮次）
    Agent,
    /// 高频调试事件
    Debug,
}

// ─── NodeEvent ────────────────────────────────────────────────

/// 节点内部事件 — 隔离 Graph 与节点内部事件的中间层。
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
    StateChange { from: String, to: String },
}

// ─── BarrierDecision ──────────────────────────────────────────

/// Barrier 审批决策。
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

// ─── GraphEvent ───────────────────────────────────────────────

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
        node_name: String,
        span_id: SpanId,
        step: usize,
    },
    /// 节点执行完成
    NodeEnd {
        node_name: String,
        span_id: SpanId,
        success: bool,
        duration: Duration,
    },
    /// 节点内部事件（通过 NodeEvent 中间层）
    Node {
        span_id: SpanId,
        node_name: String,
        event: NodeEvent,
    },
    /// Barrier 暂停 — 等待外部审批信号。
    ///
    /// ⚠️ **必须处理** — 如果不发送决策，Graph 执行将永久阻塞。
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
    /// Graph 执行完成（恰好一次）
    ///
    /// `GraphResult` 即为终态的终极真理之源——内含 `state`、`execution_log`、`duration`。
    /// 不再在外层冗余携带 `state`。
    GraphComplete { result: GraphResult },
    /// Graph 执行出错（恰好一次）
    ///
    /// 携带出错瞬间的 `state` 快照，便于诊断。
    GraphError { error: GraphError, state: State },
}

/// Graph 事件通道类型别名
pub type GraphStream = tokio::sync::mpsc::Receiver<GraphEvent>;

/// Graph 流式执行的完整返回包装。
///
/// 将 stream（观察权）、handle（控制权）封装为高内聚的结构体。
/// **Stream is primary, Blocking is derived.**
pub struct GraphExecution {
    /// 事件接收器（read-only view）
    pub stream: GraphStream,
    /// 执行句柄（write + cancel）
    pub handle: GraphHandle,
}

// ─── GraphHandle ──────────────────────────────────────────────

/// Graph 执行句柄 — 用于与运行中的 Graph 交互。
///
/// 通过 `execute_stream()` 返回，消费者使用此句柄提交 Barrier 决策或取消执行。
pub struct GraphHandle {
    decision_tx: tokio::sync::mpsc::Sender<BarrierDecisionMessage>,
    cancel_tx: tokio::sync::mpsc::Sender<()>,
}

/// 决策消息 — 支持精确匹配和通配匹配。
#[allow(dead_code)]
pub(crate) enum BarrierDecisionMessage {
    /// 精确匹配特定 BarrierId
    Exact {
        barrier_id: BarrierId,
        decision: BarrierDecision,
    },
    /// 通配匹配 — 匹配 node_id 的所有 occurrence
    Wildcard {
        node_id: String,
        decision: BarrierDecision,
    },
}

impl GraphHandle {
    pub(crate) fn new(
        decision_tx: tokio::sync::mpsc::Sender<BarrierDecisionMessage>,
        cancel_tx: tokio::sync::mpsc::Sender<()>,
    ) -> Self {
        Self {
            decision_tx,
            cancel_tx,
        }
    }

    /// 提交 Barrier 决策（精确匹配）。
    ///
    /// - `barrier_id` — 来自 `GraphEvent::BarrierWaiting` 的 ID
    /// - `decision` — 审批决策
    ///
    /// **一次性语义：** 每个 BarrierId 只能提交一次决策，重复提交返回错误。
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

    /// 提交通配决策 — 匹配指定 node_id 的所有 occurrence。
    ///
    /// 适用于"每次都 Approve"等场景。
    ///
    /// ```rust,ignore
    /// handle.decide_wildcard("approve_deploy", BarrierDecision::Approve);
    /// // 匹配 approve_deploy 的所有 occurrence
    /// ```
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

    /// 强制取消正在执行的 Graph。
    ///
    /// 发送取消信号后，executor 在主循环检测点响应：
    /// - 立即终止执行，发送 `GraphError` 事件
    /// - 如果正在等待 Barrier 决策，中断等待
    ///
    /// 多次调用安全（idempotent）。
    pub fn cancel(&self) {
        // send 失败说明 executor 已结束，忽略即可
        let _ = self.cancel_tx.try_send(());
    }
}
