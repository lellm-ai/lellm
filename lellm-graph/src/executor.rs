//! Graph 执行引擎。
//!
//! 提供阻塞执行（`execute`）与流式执行（`execute_stream`）两种模式。
//! 运行时全局步数限制（`max_steps`）防止无限循环。

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::barrier_node::BarrierDefaultAction;
use crate::error::GraphError;
use crate::event::{BarrierDecision, BarrierId, GraphEvent, GraphHandle, GraphStream, TraceId};
use crate::graph::Graph;
use crate::node::{GraphNode, NextStep, NodeKind, PendingDecisions, StreamNodeResult};
use crate::state::{ExecutionEntry, GraphResult, State};

/// 边访问计数器 — 跟踪 (from, to) 对的 traversed 次数。
#[derive(Default)]
struct EdgeVisits(HashMap<(String, String), usize>);

impl EdgeVisits {
    fn record(
        &mut self,
        from: &str,
        to: &str,
        max_visits: Option<usize>,
    ) -> Result<(), GraphError> {
        let key = (from.to_string(), to.to_string());
        let count = self.0.entry(key).or_insert(0);
        *count += 1;

        if let Some(limit) = max_visits {
            if *count > limit {
                return Err(GraphError::EdgeLimitExceeded {
                    edge: format!("{from}→{to}"),
                    limit,
                });
            }
        }
        Ok(())
    }
}

/// Graph 执行器 — 可配置运行时参数。
#[derive(Clone, Debug)]
pub struct GraphExecutor {
    /// 全局运行时步数限制。
    pub max_steps: usize,
}

impl Default for GraphExecutor {
    fn default() -> Self {
        Self { max_steps: 50 }
    }
}

impl GraphExecutor {
    pub fn new(max_steps: usize) -> Self {
        Self { max_steps }
    }

    // ─── 阻塞执行 ──────────────────────────────────────────────

    /// 执行 Graph（阻塞模式）。
    pub async fn execute(
        &self,
        graph: &Graph,
        initial_state: State,
    ) -> Result<GraphResult, GraphError> {
        let start_time = Instant::now();
        let mut state = initial_state;
        let mut execution_log = Vec::new();
        let mut edge_visits = EdgeVisits::default();

        let mut current = graph.start_node().to_string();
        let mut step: usize = 0;

        loop {
            step += 1;

            if step > self.max_steps {
                return Err(GraphError::StepsExceeded {
                    limit: self.max_steps,
                });
            }

            let node = graph
                .nodes
                .get(&current)
                .ok_or_else(|| GraphError::NodeNotFound(current.clone()))?;

            let node_start = Instant::now();
            let result = node.execute(&mut state).await;
            let node_end = Instant::now();

            execution_log.push(ExecutionEntry {
                node_name: current.clone(),
                start_time: node_start,
                end_time: node_end,
                success: result.is_ok(),
            });

            let next = result?;

            // 如果刚执行的是 end 节点，直接结束（不解析出边）
            if current == graph.end_node() {
                break;
            }

            current = self.resolve_next(graph, &current, &mut state, &mut edge_visits, next)?;
        }

        Ok(GraphResult {
            state,
            execution_log,
            duration: start_time.elapsed(),
        })
    }

    // ─── 流式执行 ──────────────────────────────────────────────

    /// 流式执行 Graph，返回事件接收器与执行句柄。
    pub fn execute_stream(
        &self,
        graph: std::sync::Arc<Graph>,
        initial_state: State,
    ) -> (GraphStream, GraphHandle) {
        let executor = self.clone();
        let (event_tx, event_rx) = mpsc::channel(32);
        let (decision_tx, mut decision_rx) = mpsc::channel(16);

        let handle = GraphHandle::new(decision_tx);

        tokio::spawn(async move {
            let start_time = Instant::now();
            let mut state = initial_state;
            let mut execution_log = Vec::new();
            let mut edge_visits = EdgeVisits::default();
            let pending_decisions: PendingDecisions =
                std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new()));

            let mut current = graph.start_node().to_string();
            let mut step: usize = 0;

            let send = |event: GraphEvent| async {
                if event_tx.send(event).await.is_err() {
                    tracing::warn!("graph event consumer dropped");
                }
            };

            loop {
                step += 1;

                // ⚡ 运行时熔断
                if step > executor.max_steps {
                    let _ = send(GraphEvent::GraphError {
                        error: GraphError::StepsExceeded {
                            limit: executor.max_steps,
                        },
                    })
                    .await;
                    break;
                }

                let node = match graph.nodes.get(&current) {
                    Some(n) => n,
                    None => {
                        let _ = send(GraphEvent::GraphError {
                            error: GraphError::NodeNotFound(current.clone()),
                        })
                        .await;
                        break;
                    }
                };

                let node_name = current.clone();
                let trace_id = TraceId::new();

                let _ = send(GraphEvent::NodeStart {
                    node_name: node_name.clone(),
                    trace_id,
                })
                .await;

                let node_start = Instant::now();
                let result = node
                    .execute_stream(&mut state, &event_tx, trace_id, pending_decisions.clone())
                    .await;
                let node_end = Instant::now();
                let duration = node_end.duration_since(node_start);

                match result {
                    Ok(StreamNodeResult::Done {
                        next,
                        trace_id: _tid,
                    }) => {
                        execution_log.push(ExecutionEntry {
                            node_name: node_name.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: true,
                        });

                        let _ = send(GraphEvent::NodeEnd {
                            node_name: node_name.clone(),
                            trace_id,
                            success: true,
                            duration,
                        })
                        .await;

                        // 如果刚执行的是 end 节点，直接结束（不解析出边）
                        if current == graph.end_node() {
                            break;
                        }

                        match executor.resolve_next(
                            &graph,
                            &current,
                            &mut state,
                            &mut edge_visits,
                            next,
                        ) {
                            Ok(target) => {
                                current = target;
                            }
                            Err(e) => {
                                let _ = send(GraphEvent::GraphError { error: e }).await;
                                break;
                            }
                        }
                    }
                    Ok(StreamNodeResult::BarrierPaused {
                        barrier_id,
                        node_name: barrier_name,
                        trace_id: _,
                        timeout,
                        default_action,
                    }) => {
                        // 发射 BarrierPaused 事件（不含 oneshot Sender）
                        let _ = send(GraphEvent::BarrierPaused {
                            barrier_id,
                            node_name: barrier_name.clone(),
                        })
                        .await;

                        // 等待决策通过 handle.decide() 到达，支持超时
                        let decision = executor
                            .wait_barrier_decision(
                                &mut decision_rx,
                                &pending_decisions,
                                barrier_id,
                                timeout,
                                &default_action,
                            )
                            .await;

                        // 应用决策
                        let next = match node {
                            NodeKind::Barrier(b) => match b.apply_decision(decision, &mut state) {
                                Ok(ns) => ns,
                                Err(e) => {
                                    let _ = send(GraphEvent::GraphError { error: e }).await;
                                    break;
                                }
                            },
                            _ => unreachable!("expected BarrierNode for BarrierPaused"),
                        };

                        execution_log.push(ExecutionEntry {
                            node_name: barrier_name.clone(),
                            start_time: node_start,
                            end_time: Instant::now(),
                            success: true,
                        });

                        let _ = send(GraphEvent::NodeEnd {
                            node_name: barrier_name.clone(),
                            trace_id,
                            success: true,
                            duration: node_end.duration_since(node_start),
                        })
                        .await;

                        // 如果刚执行的是 end 节点，直接结束
                        if current == graph.end_node() {
                            break;
                        }

                        match executor.resolve_next(
                            &graph,
                            &current,
                            &mut state,
                            &mut edge_visits,
                            next,
                        ) {
                            Ok(target) => {
                                current = target;
                            }
                            Err(e) => {
                                let _ = send(GraphEvent::GraphError { error: e }).await;
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        execution_log.push(ExecutionEntry {
                            node_name: node_name.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: false,
                        });

                        let _ = send(GraphEvent::NodeEnd {
                            node_name: node_name.clone(),
                            trace_id,
                            success: false,
                            duration,
                        })
                        .await;

                        let _ = send(GraphEvent::GraphError { error: e }).await;
                        break;
                    }
                }
            }

            let _ = send(GraphEvent::GraphComplete {
                result: GraphResult {
                    state,
                    execution_log,
                    duration: start_time.elapsed(),
                },
            })
            .await;
        });

        (event_rx, handle)
    }

    /// 等待 Barrier 决策通过 handle 到达，并转发到 pending_decisions。
    async fn wait_barrier_decision(
        &self,
        decision_rx: &mut mpsc::Receiver<(BarrierId, BarrierDecision)>,
        pending_decisions: &PendingDecisions,
        target_id: BarrierId,
        timeout: Option<std::time::Duration>,
        default_action: &BarrierDefaultAction,
    ) -> BarrierDecision {
        // 先检查是否已有决策
        {
            let map = pending_decisions.lock().await;
            if let Some(decision) = map.get(&target_id) {
                return decision.clone();
            }
        }

        if let Some(timeout) = timeout {
            let start = std::time::Instant::now();
            loop {
                // 使用 try_recv 避免阻塞，定期检查超时
                match decision_rx.try_recv() {
                    Ok((barrier_id, decision)) => {
                        pending_decisions
                            .lock()
                            .await
                            .insert(barrier_id, decision.clone());
                        if barrier_id == target_id {
                            return decision;
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        return Self::default_decision(default_action);
                    }
                }
                if start.elapsed() >= timeout {
                    tracing::warn!(
                        timeout = ?timeout,
                        action = ?default_action,
                        "barrier timeout, applying default action"
                    );
                    return Self::default_decision(default_action);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        } else {
            // 无限等待
            loop {
                if let Some((barrier_id, decision)) = decision_rx.recv().await {
                    pending_decisions
                        .lock()
                        .await
                        .insert(barrier_id, decision.clone());
                    if barrier_id == target_id {
                        return decision;
                    }
                }
            }
        }
    }

    fn default_decision(action: &BarrierDefaultAction) -> BarrierDecision {
        match action {
            BarrierDefaultAction::Approve => BarrierDecision::Approve,
            BarrierDefaultAction::Reject => BarrierDecision::Reject {
                reason: "timeout — no decision received".into(),
            },
            BarrierDefaultAction::Skip => BarrierDecision::Approve,
        }
    }

    // ─── 公共辅助 ──────────────────────────────────────────────

    /// 解析 NextStep 为目标节点名称。
    fn resolve_next(
        &self,
        graph: &Graph,
        current: &str,
        state: &mut State,
        edge_visits: &mut EdgeVisits,
        next: NextStep,
    ) -> Result<String, GraphError> {
        match next {
            NextStep::Goto(target) => {
                Self::transition(graph, current, &target, edge_visits)?;
                Ok(target)
            }
            NextStep::GoToNext => {
                let (target, max_visits) = Self::find_next_node(graph, current, state)?;
                edge_visits.record(current, &target, max_visits)?;
                Ok(target)
            }
            NextStep::End => Err(GraphError::InvalidGraph("unexpected End next step".into())),
        }
    }

    /// 统一跳转校验 — 验证边存在并记录访问计数。
    ///
    /// 所有 Goto(target) 跳转都必须对应图中的一条 Edge，否则返回 MissingEdge 错误。
    fn transition(
        graph: &Graph,
        current: &str,
        target: &str,
        edge_visits: &mut EdgeVisits,
    ) -> Result<(), GraphError> {
        let edge = graph
            .find_edge(current, target)
            .ok_or_else(|| GraphError::MissingEdge {
                from: current.to_string(),
                to: target.to_string(),
            })?;

        edge_visits.record(current, target, edge.max_visits)?;
        Ok(())
    }

    fn find_next_node(
        graph: &Graph,
        current: &str,
        state: &State,
    ) -> Result<(String, Option<usize>), GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            return Err(GraphError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            )));
        }

        for edge in &edges {
            if let Some(ref condition) = edge.condition
                && condition(state)
            {
                return Ok((edge.to.clone(), edge.max_visits));
            }
        }

        for edge in &edges {
            if edge.condition.is_none() {
                return Ok((edge.to.clone(), edge.max_visits));
            }
        }

        Err(GraphError::InvalidGraph(format!(
            "node '{}' has no matching edge. conditions: [{}]",
            current,
            edges
                .iter()
                .map(|e| {
                    let evaluated = if let Some(ref c) = e.condition {
                        format!("{}", c(state))
                    } else {
                        "default".to_string()
                    };
                    format!("{}→{}={}", e.from, e.to, evaluated)
                })
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}
