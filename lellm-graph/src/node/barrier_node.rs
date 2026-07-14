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

use super::node_context::{LeafContext, NodeContext};
#[allow(deprecated)]
use super::{FlowNode, LeafNode};
use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierId};
use crate::state::workflow_state::WorkflowState;
use crate::state::{State, StateMutation};

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
    /// 每次 execute() 递增，生成唯一的 BarrierId::occurrence。
    /// Arc 包裹确保 clone 后共享同一计数器。
    occurrence_counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
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
            occurrence_counter: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
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

/// BarrierNode 实现 LeafNode（推荐路径 — 只读 state + pause）。
#[async_trait]
impl<S: WorkflowState> LeafNode<S> for BarrierNode<S> {
    async fn execute(&self, ctx: &mut LeafContext<'_, S>) -> Result<(), GraphError> {
        let occurrence = self.occurrence_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let barrier_id = BarrierId::new(&self.name, occurrence);
        ctx.pause(barrier_id, self.timeout);
        ctx.set_has_side_effects();
        Ok(())
    }
}

#[allow(deprecated)]
#[async_trait]
impl<S: WorkflowState> FlowNode<S> for BarrierNode<S> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        let occurrence = self.occurrence_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let barrier_id = BarrierId::new(&self.name, occurrence);
        ctx.pause(barrier_id, self.timeout);
        ctx.set_has_side_effects();
        Ok(())
    }
}
