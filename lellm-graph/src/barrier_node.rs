//! Human-in-the-loop 审批节点。
//!
//! BarrierNode 在执行时暂停 Graph，通过 `GraphHandle::decide()` 等待外部决策。
//! 消费者收到 `GraphEvent::BarrierPaused` 后，通过 `GraphHandle` 发送 [`BarrierDecision`]。

use async_trait::async_trait;

use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierId};
use crate::node::FlowNode;
use crate::node_context::NodeContext;

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
/// 1. 调用 ctx.pause()，executor 发射 BarrierWaiting 事件
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

    /// 处理决策结果 — 直接写入 ctx。
    ///
    /// 由 executor 在收到外部决策后调用。
    pub fn apply_decision_to_ctx(&self, ctx: &mut NodeContext<'_>, decision: BarrierDecision) {
        match decision {
            BarrierDecision::Approve => {
                tracing::info!(barrier = %self.name, "approved");
                ctx.set(&self.approve_key, true);
                ctx.remove(&self.reject_key);
            }
            BarrierDecision::Reject { reason } => {
                tracing::warn!(barrier = %self.name, reason = %reason, "rejected");
                ctx.set(&self.reject_key, reason);
                ctx.remove(&self.approve_key);
            }
            BarrierDecision::Modify { key, value } => {
                tracing::info!(barrier = %self.name, key = %key, "state modified");
                ctx.set(&key, value);
            }
            BarrierDecision::Reroute { target } => {
                tracing::info!(barrier = %self.name, target = %target, "rerouted");
                ctx.goto(&target);
            }
        }
    }
}

#[async_trait]
impl FlowNode for BarrierNode {
    /// 执行 — 发出 pause 信号，由 executor 发射事件并等待决策。
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let barrier_id = BarrierId::new(&self.name, 0);
        ctx.pause(barrier_id, self.timeout);
        ctx.set_has_side_effects();
        Ok(())
    }
}
