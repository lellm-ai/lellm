//! Human-in-the-loop 审批节点。
//!
//! v0.4+: 泛型化 `BarrierNode<S: WorkflowState>`。
//!
//! # 限制
//!
//! `BarrierNode` 要求 `S: WorkflowState<Mutation = StateMutation>`，
//! 因为 `apply_decision_to_ctx()` 使用 key/value 操作（`Put`/`Delete`）。
//! 对于 `AgentState` 等类型化状态，请实现自定义 Barrier 节点
//! 或使用 `BarrierDecision` 枚举配合自定义逻辑。

use async_trait::async_trait;

use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierId};
use crate::node::FlowNode;
use crate::node_context::NodeContext;
use crate::state::{State, StateMutation};
use crate::workflow_state::WorkflowState;

/// Barrier 超时后的默认行为。
#[derive(Debug, Clone, Default)]
pub enum BarrierDefaultAction {
    #[default]
    Reject,
    Approve,
    Skip,
}

/// Human-in-the-loop 审批节点。
#[derive(Debug, Clone)]
pub struct BarrierNode<S: WorkflowState = State> {
    pub name: String,
    pub timeout: Option<std::time::Duration>,
    pub default_action: BarrierDefaultAction,
    pub reject_key: String,
    pub approve_key: String,
    /// Phantom 用于标记泛型类型
    _phantom: std::marker::PhantomData<S>,
}

impl<S: WorkflowState<Mutation = StateMutation>> BarrierNode<S> {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            timeout: None,
            default_action: BarrierDefaultAction::default(),
            reject_key: format!("{name}.reject_reason"),
            approve_key: format!("{name}.approved"),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn default_action(mut self, action: BarrierDefaultAction) -> Self {
        self.default_action = action;
        self
    }

    pub fn reject_key(mut self, key: impl Into<String>) -> Self {
        self.reject_key = key.into();
        self
    }

    pub fn approve_key(mut self, key: impl Into<String>) -> Self {
        self.approve_key = key.into();
        self
    }

    pub fn apply_decision_to_ctx(&self, ctx: &mut NodeContext<'_, S>, decision: BarrierDecision) {
        match decision {
            BarrierDecision::Approve => {
                tracing::info!(barrier = %self.name, "approved");
                ctx.record(StateMutation::Put(
                    self.approve_key.clone(),
                    serde_json::json!(true),
                ));
                ctx.record(StateMutation::Delete(self.reject_key.clone()));
            }
            BarrierDecision::Reject { reason } => {
                tracing::warn!(barrier = %self.name, reason = %reason, "rejected");
                ctx.record(StateMutation::Put(
                    self.reject_key.clone(),
                    serde_json::json!(reason),
                ));
                ctx.record(StateMutation::Delete(self.approve_key.clone()));
            }
            BarrierDecision::Modify { key, value } => {
                tracing::info!(barrier = %self.name, key = %key, "state modified");
                ctx.record(StateMutation::Put(key, value));
            }
            BarrierDecision::Reroute { target } => {
                tracing::info!(barrier = %self.name, target = %target, "rerouted");
                ctx.goto(&target);
            }
        }
    }
}

#[async_trait]
impl<S: WorkflowState> FlowNode<S> for BarrierNode<S> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        let barrier_id = BarrierId::new(&self.name, 0);
        ctx.pause(barrier_id, self.timeout);
        ctx.set_has_side_effects();
        Ok(())
    }
}
