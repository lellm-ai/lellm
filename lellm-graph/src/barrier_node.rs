//! Human-in-the-loop 审批节点。
//!
//! BarrierNode 在执行时暂停 Graph，通过 oneshot channel 等待外部决策。
//! 消费者收到 `GraphEvent::BarrierPaused` 后，发送 [`BarrierDecision`] 继续执行。

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::error::GraphError;
use crate::event::{BarrierDecision, GraphEvent};
use crate::node::GraphNode;
use crate::node::NextStep;
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
/// 1. 发射 `GraphEvent::BarrierPaused { signal }` 到 sink
/// 2. `tokio::select!` 等待决策信号或超时
/// 3. 根据决策写入 State，决定下一步
///
/// **阻塞模式不支持。** 调用 `execute()` 直接报错，引导使用 `execute_stream()`。
///
/// ```rust,ignore
/// // 构建包含 Barrier 的 Graph
/// let graph = GraphBuilder::new("review_flow")
///     .start("agent")
///     .node("agent", NodeKind::Agent(Box::new(agent_node)))
///     .node("review", NodeKind::Barrier(BarrierNode::new("review")
///         .timeout(Duration::from_secs(300))
///         .default_action(BarrierDefaultAction::Reject)))
///     .node("output", NodeKind::Task(output_node))
///     .edge("agent", "review")
///     .edge("review", "output")
///     .end("output")
///     .build();
///
/// // 消费事件并审批
/// let mut stream = GraphExecutor::default().execute_stream(graph, state);
/// while let Some(event) = stream.recv().await {
///     match event {
///         GraphEvent::BarrierPaused { node_name, signal } => {
///             let approved = ask_user(&node_name).await;
///             let _ = signal.send(if approved {
///                 BarrierDecision::Approve
///             } else {
///                 BarrierDecision::Reject { reason: "质量不达标".into() }
///             });
///         }
///         GraphEvent::GraphComplete { result } => {
///             println!("done: {:?}", result);
///         }
///         _ => {}
///     }
/// }
/// ```
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
    /// **幂等性保证：** Approve 时清除 reject_reason，防止 edge_if 误判回跳。
    fn apply_decision(
        &self,
        decision: BarrierDecision,
        state: &mut State,
    ) -> Result<NextStep, GraphError> {
        match decision {
            BarrierDecision::Approve => {
                tracing::info!(barrier = %self.name, "approved");
                state.insert(self.approve_key.clone(), serde_json::json!(true));
                // 清除拒绝原因，防止 edge_if 误判回跳
                state.remove(&self.reject_key);
                Ok(NextStep::GoToNext)
            }
            BarrierDecision::Reject { reason } => {
                tracing::warn!(barrier = %self.name, reason = %reason, "rejected");
                state.insert(self.reject_key.clone(), serde_json::json!(reason));
                // 清除审批标记
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

    /// 根据默认行为生成决策。
    fn default_decision(&self) -> BarrierDecision {
        match &self.default_action {
            BarrierDefaultAction::Approve => BarrierDecision::Approve,
            BarrierDefaultAction::Reject => BarrierDecision::Reject {
                reason: "timeout — no decision received".into(),
            },
            BarrierDefaultAction::Skip => BarrierDecision::Approve,
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

    /// 流式执行 — 发射 BarrierPaused 事件，等待外部决策。
    async fn execute_stream(
        &self,
        state: &mut State,
        sink: &mpsc::Sender<GraphEvent>,
    ) -> Result<NextStep, GraphError> {
        let (signal_tx, signal_rx) = oneshot::channel::<BarrierDecision>();

        // 发射暂停事件，携带 oneshot sender
        if sink
            .send(GraphEvent::BarrierPaused {
                node_name: self.name.clone(),
                signal: signal_tx,
            })
            .await
            .is_err()
        {
            return Err(GraphError::BarrierCancelled {
                node: self.name.clone(),
            });
        }

        let decision = if let Some(timeout) = self.timeout {
            // 带超时的 select
            tokio::select! {
                result = signal_rx => {
                    result.map_err(|_| GraphError::BarrierCancelled {
                        node: self.name.clone(),
                    })?
                }
                _ = tokio::time::sleep(timeout) => {
                    tracing::warn!(
                        barrier = %self.name,
                        timeout = ?timeout,
                        action = ?self.default_action,
                        "barrier timeout, applying default action"
                    );
                    self.default_decision()
                }
            }
        } else {
            // 无限等待
            signal_rx.await.map_err(|_| GraphError::BarrierCancelled {
                node: self.name.clone(),
            })?
        };

        self.apply_decision(decision, state)
    }
}
