//! Human-in-the-loop 审批节点。
//!
//! BarrierNode 在执行时暂停 Graph，通过 `GraphHandle::decide()` 等待外部决策。
//! 消费者收到 `GraphEvent::BarrierPaused` 后，通过 `GraphHandle` 发送 [`BarrierDecision`]。

use async_trait::async_trait;

use crate::error::{GraphError, TerminalError};
use crate::event::{BarrierDecision, BarrierId, GraphEvent};
use crate::node::{FlowNode, NextStep, NodeOutput, StreamNodeResult};
use crate::state::{SpanId, State, StateDelta};

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
/// 3. executor 的 `wait_barrier_decision()` 接收决策，调用 `apply_decision()` 应用
///
/// **阻塞模式不支持。** 调用 `execute()` 直接报错，引导使用 `execute_stream()`。
#[derive(Debug, Clone)]
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

    /// 处理决策结果 — 返回 NextStep + StateDelta，不直接修改 State。
    ///
    /// 由 executor 在收到外部决策后调用。Executor 负责 apply deltas。
    pub fn apply_decision(&self, decision: BarrierDecision) -> (NextStep, Vec<StateDelta>) {
        match decision {
            BarrierDecision::Approve => {
                tracing::info!(barrier = %self.name, "approved");
                let deltas = vec![
                    StateDelta::put(&self.approve_key, serde_json::json!(true)),
                    StateDelta::delete(&self.reject_key),
                ];
                (NextStep::GoToNext, deltas)
            }
            BarrierDecision::Reject { reason } => {
                tracing::warn!(barrier = %self.name, reason = %reason, "rejected");
                let deltas = vec![
                    StateDelta::put(&self.reject_key, serde_json::json!(reason)),
                    StateDelta::delete(&self.approve_key),
                ];
                (NextStep::GoToNext, deltas)
            }
            BarrierDecision::Modify { key, value } => {
                tracing::info!(barrier = %self.name, key = %key, "state modified");
                let deltas = vec![StateDelta::put(key, value)];
                (NextStep::GoToNext, deltas)
            }
            BarrierDecision::Reroute { target } => {
                tracing::info!(barrier = %self.name, target = %target, "rerouted");
                (NextStep::Goto(target), vec![])
            }
        }
    }
}

#[async_trait]
impl FlowNode for BarrierNode {
    /// 阻塞模式不支持 BarrierNode — 直接报错。
    async fn execute(&self, _state: &State) -> Result<NodeOutput, GraphError> {
        Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
            "BarrierNode '{}' requires stream mode. Use GraphExecutor::execute_stream() for human-in-the-loop.",
            self.name
        ))))
    }

    /// 流式执行 — 返回 Pause，由 executor 发射事件并等待决策。
    async fn execute_stream(
        &self,
        _state: &State,
        _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        span_id: SpanId,
    ) -> Result<StreamNodeResult, GraphError> {
        let node_name = self.name.clone();

        // barrier_id 由 executor 的 DecisionRegistry 生成
        // 这里传一个 placeholder，executor 会用 DecisionRegistry::next_id() 覆盖
        let barrier_id = BarrierId::new(&node_name, 0);

        // 返回 Pause，由 executor 发射 BarrierWaiting 事件
        Ok(StreamNodeResult::Pause {
            deltas: vec![],
            barrier_id,
            node_name,
            span_id,
            timeout: self.timeout,
            default_action: self.default_action.clone(),
        })
    }
}
