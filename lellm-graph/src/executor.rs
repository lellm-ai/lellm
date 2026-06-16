//! Graph 执行引擎。
//!
//! 提供阻塞执行（`execute`）与流式执行（`execute_stream`）两种模式。
//! 运行时全局步数限制（`max_steps`）防止无限循环。
//!
//! 流式执行返回 `GraphExecution`（stream + handle）。
//! **Stream is primary, Blocking is derived.**

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::barrier_node::BarrierDefaultAction;
use crate::error::{GraphError, TerminalError};
use crate::event::{
    BarrierDecision, BarrierDecisionMessage, BarrierId, GraphEvent, GraphExecution, GraphHandle,
};
use crate::graph::Graph;
use crate::node::{FlowNode, NextStep, NodeKind, StreamNodeResult};
use crate::state::{ExecutionEntry, GraphResult, State, SpanId, TraceId};

// ─── DecisionRegistry ─────────────────────────────────────────

/// Barrier 决策注册表 — Executor 私有状态。
///
/// Level-triggered：在 Barrier 进入等待状态之前提交的决策 MUST 被保留。
struct DecisionRegistry {
    pending: HashMap<BarrierId, BarrierDecision>,
    wildcards: HashMap<String, BarrierDecision>,
    occurrence_counter: HashMap<String, u32>,
}

impl DecisionRegistry {
    fn new() -> Self {
        Self {
            pending: HashMap::new(),
            wildcards: HashMap::new(),
            occurrence_counter: HashMap::new(),
        }
    }

    fn next_id(&mut self, node_id: &str) -> BarrierId {
        let occ = self
            .occurrence_counter
            .entry(node_id.to_string())
            .or_insert(0);
        *occ += 1;
        BarrierId::new(node_id, *occ)
    }

    fn take(&mut self, target_id: &BarrierId) -> Option<BarrierDecision> {
        if let Some(decision) = self.pending.remove(target_id) {
            return Some(decision);
        }
        self.wildcards.get(&target_id.node_id).cloned()
    }

    fn process_message(
        &mut self,
        msg: BarrierDecisionMessage,
        target_id: &BarrierId,
    ) -> Option<BarrierDecision> {
        match msg {
            BarrierDecisionMessage::Exact {
                barrier_id,
                decision,
            } => {
                if barrier_id == *target_id {
                    Some(decision)
                } else {
                    self.pending.insert(barrier_id, decision);
                    None
                }
            }
            BarrierDecisionMessage::Wildcard { node_id, decision } => {
                // 始终存储通配决策，以便后续 occurrence 使用
                self.wildcards.insert(node_id.clone(), decision.clone());
                if node_id == target_id.node_id {
                    Some(decision)
                } else {
                    None
                }
            }
        }
    }
}

// ─── GraphExecutor ────────────────────────────────────────────

/// Graph 执行器 — 可配置运行时参数。
#[derive(Clone, Debug)]
pub struct GraphExecutor {
    /// 全局运行时步数限制。
    /// 1 Step = 1 Node Entry。
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
    ///
    /// **Blocking is derived from stream.** 内部消费 stream 直到结束。
    ///
    /// ⚠️ **BarrierNode 不支持阻塞模式。** 如果图中包含 BarrierNode，
    /// 会提前返回错误，引导用户使用 `execute_stream()`。
    pub async fn execute(
        &self,
        graph: std::sync::Arc<Graph>,
        initial_state: State,
    ) -> Result<GraphResult, GraphError> {
        for (name, node) in &graph.nodes {
            if matches!(node, NodeKind::Barrier(_)) {
                return Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
                    "BarrierNode '{}' requires stream mode. Use GraphExecutor::execute_stream() for human-in-the-loop.",
                    name
                ))));
            }
        }

        let GraphExecution { mut stream, handle } = self.execute_stream(graph, initial_state);

        drop(handle);

        let mut result = None;

        while let Some(event) = stream.recv().await {
            match event {
                GraphEvent::GraphComplete { result: r } => {
                    result = Some(Ok(r));
                }
                GraphEvent::GraphError { error, .. } => {
                    result = Some(Err(error));
                }
                _ => {}
            }
        }

        result.unwrap_or_else(|| {
            Err(GraphError::Terminal(TerminalError::InvalidGraph(
                "stream ended without completion".into(),
            )))
        })
    }

    // ─── 流式执行 ──────────────────────────────────────────────

    /// 流式执行 Graph，返回 `GraphExecution`（stream + handle）。
    ///
    /// **Stream is primary, Blocking is derived.**
    pub fn execute_stream(
        &self,
        graph: std::sync::Arc<Graph>,
        initial_state: State,
    ) -> GraphExecution {
        let executor = self.clone();
        let (event_tx, event_rx) = mpsc::channel(32);
        let (decision_tx, mut decision_rx) = mpsc::channel(16);
        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);

        let handle = GraphHandle::new(decision_tx, cancel_tx);

        tokio::spawn(async move {
            let start_time = Instant::now();
            let mut state = initial_state;
            let mut execution_log = Vec::new();
            let mut decision_registry = DecisionRegistry::new();

            let mut current = graph.start_node().to_string();
            let mut step: usize = 0;

            let trace_id = TraceId::default();

            // 发射 GraphStart 事件
            if executor
                .send(&event_tx, GraphEvent::GraphStart { trace_id })
                .await
            {
                return;
            }

            let mut completed = false;

            loop {
                // ⚡ 取消信号检测
                if cancel_rx.try_recv().is_ok() {
                    if executor
                        .send(
                            &event_tx,
                            GraphEvent::GraphError {
                                error: GraphError::Terminal(TerminalError::BarrierCancelled {
                                    node: "execution cancelled by handle".into(),
                                }),
                                state: state.clone(),
                            },
                        )
                        .await
                    {
                        return;
                    }
                    break;
                }

                step += 1;

                // ⚡ 运行时熔断
                if step > executor.max_steps {
                    if executor
                        .send(
                            &event_tx,
                            GraphEvent::GraphError {
                                error: GraphError::Terminal(TerminalError::StepsExceeded {
                                    limit: executor.max_steps,
                                }),
                                state: state.clone(),
                            },
                        )
                        .await
                    {
                        return;
                    }
                    break;
                }

                let node = match graph.nodes.get(&current) {
                    Some(n) => n,
                    None => {
                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::GraphError {
                                    error: GraphError::Terminal(TerminalError::NodeNotFound(
                                        current.clone(),
                                    )),
                                    state: state.clone(),
                                },
                            )
                            .await
                        {
                            return;
                        }
                        break;
                    }
                };

                let node_name = current.clone();
                let span_id = SpanId::new();

                if executor
                    .send(
                        &event_tx,
                        GraphEvent::NodeStart {
                            node_name: node_name.clone(),
                            trace_id,
                            span_id,
                            step,
                        },
                    )
                    .await
                {
                    return;
                }

                let node_start = Instant::now();
                let result = node.execute_stream(&mut state, &event_tx, span_id).await;
                let node_end = Instant::now();
                let duration = node_end.duration_since(node_start);

                match result {
                    Ok(StreamNodeResult::Continue { next, span_id: _, observed }) => {
                        execution_log.push(ExecutionEntry {
                            step,
                            node_name: node_name.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: true,
                        });

                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::NodeEnd {
                                    node_name: node_name.clone(),
                                    trace_id,
                                    span_id,
                                    success: true,
                                    duration,
                                },
                            )
                            .await
                        {
                            return;
                        }

                        // 如果有观测错误，发送 ObservedError 事件
                        if let Some(error) = observed {
                            if executor
                                .send(
                                    &event_tx,
                                    GraphEvent::ObservedError {
                                        error,
                                        node_name: node_name.clone(),
                                    },
                                )
                                .await
                            {
                                return;
                            }
                        }

                        // 🛑 end 节点检查
                        if current == graph.end_node() {
                            completed = true;
                            break;
                        }

                        match executor.resolve_next(&graph, &current, &mut state, next) {
                            Ok(target) => current = target,
                            Err(e) => {
                                if executor
                                    .send(
                                        &event_tx,
                                        GraphEvent::GraphError {
                                            error: e,
                                            state: state.clone(),
                                        },
                                    )
                                    .await
                                {
                                    return;
                                }
                                break;
                            }
                        }
                    }

                    Ok(StreamNodeResult::Pause {
                        barrier_id: _,
                        node_name: barrier_name,
                        span_id: _,
                        timeout,
                        default_action,
                    }) => {
                        let barrier_id = decision_registry.next_id(&barrier_name);

                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::BarrierWaiting {
                                    barrier_id: barrier_id.clone(),
                                    node_name: barrier_name.clone(),
                                    span_id,
                                },
                            )
                            .await
                        {
                            return;
                        }

                        let decision = executor
                            .wait_barrier_decision(
                                &mut decision_rx,
                                &mut decision_registry,
                                &barrier_id,
                                timeout,
                                &default_action,
                                &mut cancel_rx,
                            )
                            .await;

                        if cancel_rx.try_recv().is_ok() {
                            if executor
                                .send(
                                    &event_tx,
                                    GraphEvent::GraphError {
                                        error: GraphError::Terminal(
                                            TerminalError::BarrierCancelled {
                                                node: barrier_name.clone(),
                                            },
                                        ),
                                        state: state.clone(),
                                    },
                                )
                                .await
                            {
                                return;
                            }
                            break;
                        }

                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::BarrierResolved {
                                    barrier_id: barrier_id.clone(),
                                    decision: decision.clone(),
                                },
                            )
                            .await
                        {
                            return;
                        }

                        let next = match node {
                            NodeKind::Barrier(b) => match b.apply_decision(decision, &mut state) {
                                Ok(ns) => ns,
                                Err(e) => {
                                    if executor
                                        .send(
                                            &event_tx,
                                            GraphEvent::GraphError {
                                                error: e,
                                                state: state.clone(),
                                            },
                                        )
                                        .await
                                    {
                                        return;
                                    }
                                    break;
                                }
                            },
                            _ => {
                                if executor.send(&event_tx, GraphEvent::GraphError {
                                        error: GraphError::Terminal(TerminalError::InvalidGraph(
                                            format!(
                                                "expected BarrierNode but got unexpected node type for BarrierPaused"
                                            ),
                                        )),
                                        state: state.clone(),
                                    }).await { return; }
                                break;
                            }
                        };

                        execution_log.push(ExecutionEntry {
                            step,
                            node_name: barrier_name.clone(),
                            start_time: node_start,
                            end_time: Instant::now(),
                            success: true,
                        });

                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::NodeEnd {
                                    node_name: barrier_name.clone(),
                                    trace_id,
                                    span_id,
                                    success: true,
                                    duration: Instant::now().duration_since(node_start),
                                },
                            )
                            .await
                        {
                            return;
                        }

                        // 🛑 end 节点检查
                        if current == graph.end_node() {
                            completed = true;
                            break;
                        }

                        match executor.resolve_next(&graph, &current, &mut state, next) {
                            Ok(target) => current = target,
                            Err(e) => {
                                if executor
                                    .send(
                                        &event_tx,
                                        GraphEvent::GraphError {
                                            error: e,
                                            state: state.clone(),
                                        },
                                    )
                                    .await
                                {
                                    return;
                                }
                                break;
                            }
                        }
                    }

                    Ok(StreamNodeResult::Fallback { reason, node_name: fallback_node }) => {
                        // Fallback 是控制流 — 节点主动声明降级
                        // 查找 fallback 边，路由到备用节点
                        execution_log.push(ExecutionEntry {
                            step,
                            node_name: fallback_node.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: false,
                        });

                        if let Some(fallback_target) = graph.find_fallback_edge(&current) {
                            if executor
                                .send(
                                    &event_tx,
                                    GraphEvent::ObservedError {
                                        error: crate::error::ObservedError::Degraded {
                                            node: fallback_node.clone(),
                                            message: format!("fallback to '{}': {}", fallback_target, reason),
                                        },
                                        node_name: fallback_node.clone(),
                                    },
                                )
                                .await
                            {
                                return;
                            }
                            current = fallback_target;
                        } else {
                            // 无 fallback 边 → 终止
                            if executor
                                .send(
                                    &event_tx,
                                    GraphEvent::GraphError {
                                        error: GraphError::Terminal(
                                            TerminalError::NodeExecutionFailed {
                                                node: fallback_node.clone(),
                                                source: format!("fallback with no fallback edge: {}", reason).into(),
                                            },
                                        ),
                                        state: state.clone(),
                                    },
                                )
                                .await
                            {
                                return;
                            }
                            break;
                        }
                    }

                    Err(e) => {
                        execution_log.push(ExecutionEntry {
                            step,
                            node_name: node_name.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: false,
                        });

                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::NodeEnd {
                                    node_name: node_name.clone(),
                                    trace_id,
                                    span_id,
                                    success: false,
                                    duration,
                                },
                            )
                            .await
                        {
                            return;
                        }

                        // 错误处理：Terminal → 终止执行
                        if executor
                            .send(
                                &event_tx,
                                GraphEvent::GraphError {
                                    error: e,
                                    state: state.clone(),
                                },
                            )
                            .await
                        {
                            return;
                        }
                        break;
                    }
                }
            }

            // 正常结束 → GraphComplete
            if completed {
                let _ = executor
                    .send(
                        &event_tx,
                        GraphEvent::GraphComplete {
                            result: GraphResult {
                                trace_id,
                                state,
                                execution_log,
                                duration: start_time.elapsed(),
                            },
                        },
                    )
                    .await;
            }
        });

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }

    /// 等待 Barrier 决策（支持取消信号）。
    async fn wait_barrier_decision(
        &self,
        decision_rx: &mut mpsc::Receiver<BarrierDecisionMessage>,
        registry: &mut DecisionRegistry,
        target_id: &BarrierId,
        timeout: Option<std::time::Duration>,
        default_action: &BarrierDefaultAction,
        cancel_rx: &mut mpsc::Receiver<()>,
    ) -> BarrierDecision {
        if let Some(decision) = registry.take(target_id) {
            return decision;
        }

        while let Ok(msg) = decision_rx.try_recv() {
            if let Some(decision) = registry.process_message(msg, target_id) {
                return decision;
            }
        }

        if cancel_rx.try_recv().is_ok() {
            return Self::default_decision(default_action);
        }

        if let Some(timeout) = timeout {
            let start = std::time::Instant::now();
            loop {
                match tokio::time::timeout(std::time::Duration::from_millis(50), decision_rx.recv())
                    .await
                {
                    Ok(Some(msg)) => {
                        if let Some(decision) = registry.process_message(msg, target_id) {
                            return decision;
                        }
                    }
                    Ok(None) => return Self::default_decision(default_action),
                    Err(_) => {}
                }
                if cancel_rx.try_recv().is_ok() {
                    return Self::default_decision(default_action);
                }
                if start.elapsed() >= timeout {
                    return Self::default_decision(default_action);
                }
            }
        } else {
            loop {
                if let Some(msg) = decision_rx.recv().await {
                    if let Some(decision) = registry.process_message(msg, target_id) {
                        return decision;
                    }
                } else {
                    return Self::default_decision(default_action);
                }
                if cancel_rx.try_recv().is_ok() {
                    return Self::default_decision(default_action);
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

    // ─── 路由解析 ──────────────────────────────────────────────

    /// 解析 NextStep 为目标节点名称。
    fn resolve_next(
        &self,
        graph: &Graph,
        current: &str,
        state: &mut State,
        next: NextStep,
    ) -> Result<String, GraphError> {
        match next {
            NextStep::Goto(target) => {
                // 验证边存在
                graph.find_edge(current, &target).ok_or_else(|| {
                    GraphError::Terminal(TerminalError::MissingEdge {
                        from: current.to_string(),
                        to: target.clone(),
                    })
                })?;
                Ok(target)
            }
            NextStep::GoToNext => Self::find_next_node(graph, current, state),
            NextStep::End => Err(GraphError::Terminal(TerminalError::InvalidGraph(
                "unexpected End next step".into(),
            ))),
        }
    }

    /// 查找下一个节点（三类边 + 有序路由）。
    ///
    /// 路由规则（first match wins）：
    /// 1. 条件边 — 按注册顺序求值，第一条命中即停止（if/else-if 语义）
    /// 2. 普通边 — 无条件非 fallback，取第一条
    /// 3. Fallback 边 — 无条件 fallback，取第一条
    /// 4. 无匹配 → Unrouted TerminalError
    fn find_next_node(graph: &Graph, current: &str, state: &State) -> Result<String, GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            return Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            ))));
        }

        // 1. 条件边 — 按注册顺序求值，first match wins
        for edge in &edges {
            if edge.is_conditional() && edge.condition.as_ref().is_some_and(|c| c(state)) {
                return Ok(edge.to.clone());
            }
        }

        // 2. 普通边 — 无条件非 fallback，取第一条
        for edge in &edges {
            if edge.is_normal() {
                return Ok(edge.to.clone());
            }
        }

        // 3. Fallback 边 — 无条件 fallback，取第一条
        for edge in &edges {
            if edge.fallback {
                return Ok(edge.to.clone());
            }
        }

        // 4. 无匹配 → Unrouted
        let attempted: Vec<crate::error::ConditionEval> = edges
            .iter()
            .map(|e| crate::error::ConditionEval {
                edge: format!("{}→{}", e.from, e.to),
                condition: e.condition.as_ref().map(|_| "condition".to_string()),
                matched: e.condition.as_ref().is_some_and(|c| c(state)),
            })
            .collect();

        Err(GraphError::Terminal(TerminalError::Unrouted {
            node: current.to_string(),
            attempted_conditions: attempted,
        }))
    }

    /// 发送事件，返回 `true` 表示 consumer 已断开（应终止执行）。
    ///
    /// Consumer Drop = Cancel：一旦 `send` 失败，立即终止执行，不再继续。
    async fn send(&self, event_tx: &mpsc::Sender<GraphEvent>, event: GraphEvent) -> bool {
        event_tx.send(event).await.is_err()
    }
}
