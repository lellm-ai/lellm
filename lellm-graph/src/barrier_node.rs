//! Human-in-the-loop 审批节点。
//!
//! BarrierNode 在执行时暂停 Graph，通过 `GraphHandle::decide()` 等待外部决策。
//! 消费者收到 `GraphEvent::BarrierPaused` 后，通过 `GraphHandle` 发送 [`BarrierDecision`]。

use async_trait::async_trait;

use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierId, GraphEvent, TraceId};
use crate::node::{GraphNode, NextStep, PendingDecisions, StreamNodeResult};
use crate::state::State;

/// Barrier 超时后的默认行为。
#[derive(Debug, Clone, Default)]
pub enum BarrierDefaultAction {
    /// 超时视为拒绝
    #[default]
    Reject,
    /// 超时视为通过
    Approve,
    /// 超时跳过（继续下一步）
    Skip,
}

/// Human-in-the-loop 审批节点。
///
/// 执行流程：
/// 1. 返回 `StreamNodeResult::BarrierPaused`，executor 发射 `BarrierPaused` 事件
/// 2. 消费者通过 `GraphHandle::decide(barrier_id, decision)` 提交决策
/// 3. BarrierNode 从 `pending_decisions` 获取决策，应用并返回
///
/// **阻塞模式不支持。** 调用 `execute()` 直接报错，引导使用 `execute_stream()`。
pub struct BarrierNode {
    pub name: String,
    /// 超时时间（None = 无限等待）
    pub timeout: Option<std::time::Duration>,
    /// 超时默认行为
    pub default_action: BarrierDefaultAction,
    /// 拒绝原因写入 State 的 key 后缀（默认 "{name}.reject_reason"）
    pub reject_key: String,
    /// 审批通过后写入 State 的标记 key（默认 "{name}.approved"）
    pub approve_key: String,
}

impl BarrierNode {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            timeout: None,
            default_action: BarrierDefaultAction::default(),
            reject_key: format!("{name}.reject_reason"),
            approve_key: format!("{name}.approved"),
        }
    }

    /// 设置超时时间。超时后按 `default_action` 处理。
    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// 设置超时默认行为（默认 Reject）。
    pub fn default_action(mut self, action: BarrierDefaultAction) -> Self {
        self.default_action = action;
        self
    }

    /// 设置拒绝原因写入 State 的 key（默认 "{name}.reject_reason"）。
    pub fn reject_key(mut self, key: impl Into<String>) -> Self {
        self.reject_key = key.into();
        self
    }

    /// 设置审批标记写入 State 的 key（默认 "{name}.approved"）。
    pub fn approve_key(mut self, key: impl Into<String>) -> Self {
        self.approve_key = key.into();
        self
    }

    /// 处理决策结果 — 写入 State 并返回 NextStep。
    ///
    /// 由 executor 在收到外部决策后调用。
    pub fn apply_decision(
        &self,
        decision: BarrierDecision,
        state: &mut State,
    ) -> Result<NextStep, GraphError> {
        match decision {
            BarrierDecision::Approve => {
                tracing::info!(barrier = %self.name, "approved");
                state.insert(self.approve_key.clone(), serde_json::json!(true));
                state.remove(&self.reject_key);
                Ok(NextStep::GoToNext)
            }
            BarrierDecision::Reject { reason } => {
                tracing::warn!(barrier = %self.name, reason = %reason, "rejected");
                state.insert(self.reject_key.clone(), serde_json::json!(reason));
                state.remove(&self.approve_key);
                Ok(NextStep::GoToNext)
            }
            BarrierDecision::Modify { key, value } => {
                tracing::info!(barrier = %self.name, key = %key, "state modified");
                state.insert(key, value);
                Ok(NextStep::GoToNext)
            }
            BarrierDecision::Reroute { target } => {
                tracing::info!(barrier = %self.name, target = %target, "rerouted");
                Ok(NextStep::Goto(target))
            }
        }
    }

 }

#[async_trait]
impl GraphNode for BarrierNode {
    /// 阻塞模式不支持 BarrierNode — 直接报错。
    async fn execute(&self, _state: &mut State) -> Result<NextStep, GraphError> {
        Err(GraphError::InvalidGraph(format!(
            "BarrierNode '{}' requires stream mode. Use GraphExecutor::execute_stream() for human-in-the-loop.",
            self.name
        )))
    }

    /// 流式执行 — 返回 BarrierPaused，由 executor 发射事件并等待决策。
    async fn execute_stream(
        &self,
        _state: &mut State,
        _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        trace_id: TraceId,
        _pending_decisions: PendingDecisions,
    ) -> Result<StreamNodeResult, GraphError> {
        let barrier_id = BarrierId::new();
        let node_name = self.name.clone();

        // 返回 BarrierPaused，由 executor 发射 BarrierPaused 事件
        Ok(StreamNodeResult::BarrierPaused {
            barrier_id,
            node_name,
            trace_id,
            timeout: self.timeout,
            default_action: self.default_action.clone(),
        })
    }
}
