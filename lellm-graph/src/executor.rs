//! Graph 执行引擎。
//!
//! 提供阻塞执行（`execute`）与流式执行（`execute_stream`）两种模式。
//! 运行时全局步数限制（`max_steps`）防止无限循环。
//!
//! 流式执行返回 `(GraphStream, GraphHandle)`。
//! **Stream is primary, Blocking is derived.**

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::barrier_node::BarrierDefaultAction;
use crate::error::{GraphError, ObservedError, TerminalError};
use crate::event::{
    BarrierDecision, BarrierDecisionMessage, BarrierId, GraphEvent, GraphExecution, GraphHandle,
    SpanId,
};
use crate::graph::{EdgeExceededStrategy, EdgePolicy, Graph};
use crate::node::{GraphNode, NextStep, NodeKind, StreamNodeResult};
use crate::state::{ExecutionEntry, GraphResult, State};

// ─── DecisionRegistry ─────────────────────────────────────────

/// Barrier 决策注册表 — Executor 私有状态。
///
/// Level-triggered：在 Barrier 进入等待状态之前提交的决策 MUST 被保留。
struct DecisionRegistry {
    pending: HashMap<BarrierId, BarrierDecision>,
    wildcards: HashMap<String, BarrierDecision>,
    /// 记录每个 barrier node_id 的 occurrence 计数
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

    /// 为 Barrier 生成下一个 BarrierId。
    fn next_id(&mut self, node_id: &str) -> BarrierId {
        let occ = self.occurrence_counter.entry(node_id.to_string()).or_insert(0);
        *occ += 1;
        BarrierId::new(node_id, *occ)
    }

    /// 缓存一条精确决策。
    fn insert_exact(&mut self, barrier_id: BarrierId, decision: BarrierDecision) {
        self.pending.insert(barrier_id, decision);
    }

    /// 缓存一条通配决策。
    fn insert_wildcard(&mut self, node_id: String, decision: BarrierDecision) {
        self.wildcards.insert(node_id, decision);
    }

    /// 尝试取出目标 Barrier 的决策。
    /// 先查精确匹配，再查通配匹配。
    fn take(&mut self, target_id: &BarrierId) -> Option<BarrierDecision> {
        // 1. 精确匹配
        if let Some(decision) = self.pending.remove(target_id) {
            return Some(decision);
        }
        // 2. 通配匹配
        self.wildcards.get(&target_id.node_id).cloned()
    }

    /// 处理收到的决策消息。
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
                    self.insert_exact(barrier_id, decision);
                    None
                }
            }
            BarrierDecisionMessage::Wildcard { node_id, decision } => {
                if node_id == target_id.node_id {
                    Some(decision)
                } else {
                    self.insert_wildcard(node_id, decision);
                    None
                }
            }
        }
    }
}

// ─── EdgeVisits ───────────────────────────────────────────────

/// 边跳转结果 — 区分正常跳转、策略超限、静默跳过。
#[derive(Debug)]
enum EdgeTransitionResult {
    /// 跳转成功
    Ok,
    /// 策略超限 — 严格模式，路径失败
    PolicyExceededStrict { edge: String, limit: usize },
    /// 策略超限 — 软降级，触发 fallback
    PolicyExceededSoftFallback,
    /// 策略超限 — 静默跳过
    Dropped,
}

/// 边访问计数器 — 跟踪 (from, to) 对的 traversed 次数。
/// 仅对设置了 EdgePolicy 的边进行运行时拦截。
#[derive(Default)]
struct EdgeVisits(HashMap<(String, String), usize>);

impl EdgeVisits {
    fn record(
        &mut self,
        from: &str,
        to: &str,
        policy: Option<&crate::graph::EdgePolicy>,
    ) -> EdgeTransitionResult {
        let key = (from.to_string(), to.to_string());
        let count = self.0.entry(key).or_insert(0);
        *count += 1;

        if let Some(EdgePolicy::MaxVisits { limit, on_exceeded }) = policy {
            if *count > *limit {
                return match on_exceeded {
                    EdgeExceededStrategy::Strict => EdgeTransitionResult::PolicyExceededStrict {
                        edge: format!("{from}→{to}"),
                        limit: *limit,
                    },
                    EdgeExceededStrategy::SoftFallback => {
                        EdgeTransitionResult::PolicyExceededSoftFallback
                    }
                    EdgeExceededStrategy::Drop => EdgeTransitionResult::Dropped,
                };
            }
        }
        EdgeTransitionResult::Ok
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
    /// 接收 `Arc<Graph>` 以避免克隆。与 `execute_stream()` 共享所有权模型。
    ///
    /// ⚠️ **BarrierNode 不支持阻塞模式。** 如果图中包含 BarrierNode，
    /// 会提前返回错误，引导用户使用 `execute_stream()`。
    pub async fn execute(
        &self,
        graph: std::sync::Arc<Graph>,
        initial_state: State,
    ) -> Result<GraphResult, GraphError> {
        // 提前检查 BarrierNode — 阻塞模式无法处理外部决策
        for (name, node) in &graph.nodes {
            if matches!(node, NodeKind::Barrier(_)) {
                return Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
                    "BarrierNode '{}' requires stream mode. Use GraphExecutor::execute_stream() for human-in-the-loop.",
                    name
                ))));
            }
        }

        let GraphExecution { mut stream, handle } =
            self.execute_stream(graph, initial_state);

        // 阻塞模式：handle 立即 drop，不暴露 cancel 能力
        // cancel_tx 随 handle 一起 drop，但不触发取消（executor 持有 rx）
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
            Err(GraphError::Terminal(
                TerminalError::InvalidGraph("stream ended without completion".into()),
            ))
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
            let mut edge_visits = EdgeVisits::default();
            let mut decision_registry = DecisionRegistry::new();

            let mut current = graph.start_node().to_string();
            let mut step: usize = 0;

            let send = |event: GraphEvent| async {
                if event_tx.send(event).await.is_err() {
                    tracing::warn!("graph event consumer dropped");
                }
            };

            // true = 正常完成（应发送 GraphComplete），false = 错误终止（已发送 GraphError）
            let mut completed = false;

            loop {
                // ── 循环顶部统一检查 ──────────────────────────────

                // ⚡ 取消信号检测
                if cancel_rx.try_recv().is_ok() {
                    let _ = send(
                        GraphEvent::GraphError {
                            error: GraphError::Terminal(TerminalError::BarrierCancelled {
                                node: "execution cancelled by handle".into(),
                            }),
                            state: state.clone(),
                        },
                    )
                    .await;
                    break;
                }

                step += 1;

                // ⚡ 运行时熔断
                if step > executor.max_steps {
                    let _ = send(
                        GraphEvent::GraphError {
                            error: GraphError::Terminal(TerminalError::StepsExceeded {
                                limit: executor.max_steps,
                            }),
                            state: state.clone(),
                        },
                    )
                    .await;
                    break;
                }

                let node = match graph.nodes.get(&current) {
                    Some(n) => n,
                    None => {
                        let _ = send(
                            GraphEvent::GraphError {
                                error: GraphError::Terminal(TerminalError::NodeNotFound(
                                    current.clone(),
                                )),
                                state: state.clone(),
                            },
                        )
                        .await;
                        break;
                    }
                };

                let node_name = current.clone();
                let span_id = SpanId::new();

                let _ = send(
                    GraphEvent::NodeStart {
                        node_name: node_name.clone(),
                        span_id,
                        step,
                    },
                )
                .await;

                let node_start = Instant::now();
                let result = node.execute_stream(&mut state, &event_tx, span_id).await;
                let node_end = Instant::now();
                let duration = node_end.duration_since(node_start);

                match result {
                    Ok(StreamNodeResult::Done { next, span_id: _ }) => {
                        execution_log.push(ExecutionEntry {
                            node_name: node_name.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: true,
                        });

                        let _ = send(
                            GraphEvent::NodeEnd {
                                node_name: node_name.clone(),
                                span_id,
                                success: true,
                                duration,
                            },
                        )
                        .await;

                        // 🛑 end 节点检查（统一收拢点）
                        if current == graph.end_node() {
                            completed = true;
                            break;
                        }

                        match executor.resolve_next(
                            &graph,
                            &current,
                            &mut state,
                            &mut edge_visits,
                            next,
                        ) {
                            Ok(target) => current = target,
                            Err(e) => {
                                let _ = send(GraphEvent::GraphError { error: e, state: state.clone() }).await;
                                break;
                            }
                        }
                    }

                    Ok(StreamNodeResult::Observed {
                        error,
                        next,
                        span_id: _,
                    }) => {
                        execution_log.push(ExecutionEntry {
                            node_name: node_name.clone(),
                            start_time: node_start,
                            end_time: node_end,
                            success: true,
                        });

                        let _ = send(
                            GraphEvent::NodeEnd {
                                node_name: node_name.clone(),
                                span_id,
                                success: true,
                                duration,
                            },
                        )
                        .await;

                        let _ = send(GraphEvent::ObservedError {
                            error,
                            node_name: node_name.clone(),
                        })
                        .await;

                        // 🛑 end 节点检查
                        if current == graph.end_node() {
                            completed = true;
                            break;
                        }

                        match executor.resolve_next(
                            &graph,
                            &current,
                            &mut state,
                            &mut edge_visits,
                            next,
                        ) {
                            Ok(target) => current = target,
                            Err(e) => {
                                let _ = send(GraphEvent::GraphError { error: e, state: state.clone() }).await;
                                break;
                            }
                        }
                    }

                    Ok(StreamNodeResult::BarrierPaused {
                        barrier_id: _, // 由 registry 生成
                        node_name: barrier_name,
                        span_id: _,
                        timeout,
                        default_action,
                    }) => {
                        // 生成 BarrierId
                        let barrier_id = decision_registry.next_id(&barrier_name);

                        // 发射 BarrierWaiting 事件
                        let _ = send(
                            GraphEvent::BarrierWaiting {
                                barrier_id: barrier_id.clone(),
                                node_name: barrier_name.clone(),
                                span_id,
                            },
                        )
                        .await;

                        // 等待决策（检测取消信号）
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

                        // 检查取消信号（独立于决策结果）
                        if cancel_rx.try_recv().is_ok() {
                            let _ = send(
                                GraphEvent::GraphError {
                                    error: GraphError::Terminal(TerminalError::BarrierCancelled {
                                        node: barrier_name.clone(),
                                    }),
                                    state: state.clone(),
                                },
                            )
                            .await;
                            break;
                        }

                        // 发射 BarrierResolved 事件
                        let _ = send(
                            GraphEvent::BarrierResolved {
                                barrier_id: barrier_id.clone(),
                                decision: decision.clone(),
                            },
                        )
                        .await;

                        // 应用决策
                        let next = match node {
                            NodeKind::Barrier(b) => match b.apply_decision(decision, &mut state) {
                                Ok(ns) => ns,
                                Err(e) => {
                                    let _ =
                                        send(GraphEvent::GraphError { error: e, state: state.clone() })
                                            .await;
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

                        let _ = send(
                            GraphEvent::NodeEnd {
                                node_name: barrier_name.clone(),
                                span_id,
                                success: true,
                                duration: Instant::now().duration_since(node_start),
                            },
                        )
                        .await;

                        // 🛑 end 节点检查
                        if current == graph.end_node() {
                            completed = true;
                            break;
                        }

                        match executor.resolve_next(
                            &graph,
                            &current,
                            &mut state,
                            &mut edge_visits,
                            next,
                        ) {
                            Ok(target) => current = target,
                            Err(e) => {
                                let _ = send(GraphEvent::GraphError { error: e, state: state.clone() }).await;
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

                        let _ = send(
                            GraphEvent::NodeEnd {
                                node_name: node_name.clone(),
                                span_id,
                                success: false,
                                duration,
                            },
                        )
                        .await;

                        // 🌟 错误三分法：真正区分 Terminal / Recoverable / Observed
                        match &e {
                            GraphError::Terminal(_) => {
                                let _ = send(GraphEvent::GraphError { error: e, state: state.clone() }).await;
                                break;
                            }
                            GraphError::Recoverable(recoverable) => {
                                // Recoverable：尝试 fallback 路径
                                tracing::warn!(
                                    node = %node_name,
                                    error = %recoverable,
                                    "Recoverable error captured. Attempting fallback route..."
                                );

                                if let Some(fallback_target) = graph.find_fallback_edge(&current) {
                                    // 发送 FallbackTriggered 事件
                                    let _ = send(
                                        GraphEvent::ObservedError {
                                            error: ObservedError::Degraded {
                                                node: node_name.clone(),
                                                message: format!(
                                                    "fallback to '{}' due to: {}",
                                                    fallback_target, recoverable
                                                ),
                                            },
                                            node_name: node_name.clone(),
                                        },
                                    )
                                    .await;

                                    // 跳转到 fallback 目标
                                    current = fallback_target;
                                } else {
                                    // 无 fallback 边 → 降级为 Terminal
                                    let _ = send(
                                        GraphEvent::GraphError {
                                            error: GraphError::Terminal(
                                                TerminalError::NodeExecutionFailed {
                                                    node: node_name.clone(),
                                                    source: format!(
                                                        "Recoverable error with no fallback edge: {}",
                                                        recoverable
                                                    )
                                                    .into(),
                                                },
                                            ),
                                            state: state.clone(),
                                        },
                                    )
                                    .await;
                                    break;
                                }
                            }
                            GraphError::Observed(observed) => {
                                // Observed：发射事件，不影响控制流
                                let _ = send(
                                    GraphEvent::ObservedError {
                                        error: observed.clone(),
                                        node_name: node_name.clone(),
                                    },
                                )
                                .await;
                                // 继续执行下一个节点（不 break）
                                // 注：节点返回 Err(Observed) 是不规范的，
                                // 应使用 StreamNodeResult::Observed 变体。
                                // 这里作为兼容处理。
                                if current == graph.end_node() {
                                    completed = true;
                                    break;
                                }
                                match executor.resolve_next(
                                    &graph,
                                    &current,
                                    &mut state,
                                    &mut edge_visits,
                                    NextStep::GoToNext,
                                ) {
                                    Ok(target) => current = target,
                                    Err(e) => {
                                        let _ = send(GraphEvent::GraphError { error: e, state: state.clone() }).await;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // 正常结束 → GraphComplete（仅携带 GraphResult，无冗余 state）
            if completed {
                let _ = send(
                    GraphEvent::GraphComplete {
                        result: GraphResult {
                            state,
                            execution_log,
                            duration: start_time.elapsed(),
                        },
                    },
                )
                .await;
            }
        });

        GraphExecution { stream: event_rx, handle }
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
        // 1. 先查缓存
        if let Some(decision) = registry.take(target_id) {
            return decision;
        }

        // 2. drain channel 中已有的消息
        while let Ok(msg) = decision_rx.try_recv() {
            if let Some(decision) = registry.process_message(msg, target_id) {
                return decision;
            }
        }

        // 3. 检查取消信号
        if cancel_rx.try_recv().is_ok() {
            return Self::default_decision(default_action);
        }

        // 4. 超时分支
        if let Some(timeout) = timeout {
            let start = std::time::Instant::now();
            loop {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    decision_rx.recv(),
                )
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
                // 检查取消信号
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
                // 检查取消信号
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
    ///
    /// 处理 EdgeTransitionResult 的所有变体，包括 Recoverable 的 fallback 逻辑。
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
                match Self::transition(graph, current, &target, edge_visits)? {
                    EdgeTransitionResult::Ok => Ok(target),
                    EdgeTransitionResult::PolicyExceededStrict { edge, limit } => {
                        Err(GraphError::Terminal(TerminalError::EdgePolicyExceeded { edge, limit }))
                    }
                    EdgeTransitionResult::PolicyExceededSoftFallback { .. } => {
                        if let Some(fallback_target) = graph.find_fallback_edge(current) {
                            Ok(fallback_target)
                        } else {
                            Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                                node: current.to_string(),
                                source:
                                    "SoftFallback triggered but no fallback edge defined".into(),
                            }))
                        }
                    }
                    EdgeTransitionResult::Dropped => {
                        Err(GraphError::Terminal(TerminalError::InvalidGraph(
                            "edge transition dropped for Goto".into(),
                        )))
                    }
                }
            }
            NextStep::GoToNext => {
                let (target, policy) = Self::find_next_node(graph, current, state)?;
                let result = edge_visits.record(current, &target, policy);
                match result {
                    EdgeTransitionResult::Ok => Ok(target),
                    EdgeTransitionResult::PolicyExceededStrict { edge, limit } => {
                        Err(GraphError::Terminal(TerminalError::EdgePolicyExceeded { edge, limit }))
                    }
                    EdgeTransitionResult::PolicyExceededSoftFallback { .. } => {
                        if let Some(fallback_target) = graph.find_fallback_edge(current) {
                            Ok(fallback_target)
                        } else {
                            Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                                node: current.to_string(),
                                source:
                                    "SoftFallback triggered but no fallback edge defined".into(),
                            }))
                        }
                    }
                    EdgeTransitionResult::Dropped => {
                        Self::find_fallback_or_any(graph, current, state)
                    }
                }
            }
            NextStep::End => {
                Err(GraphError::Terminal(TerminalError::InvalidGraph(
                    "unexpected End next step".into(),
                )))
            }
        }
    }

    /// 当边被 Drop 时，寻找 fallback 或任意可用边。
    fn find_fallback_or_any(
        graph: &Graph,
        current: &str,
        state: &State,
    ) -> Result<String, GraphError> {
        let edges = graph.edges_from(current);

        // 1. 先找 fallback 边
        for edge in &edges {
            if edge.fallback && (edge.condition.is_none() || edge.condition.as_ref().is_some_and(|c| c(state))) {
                return Ok(edge.to.clone());
            }
        }

        // 2. 再找无条件 fallback
        for edge in &edges {
            if edge.fallback && edge.condition.is_none() {
                return Ok(edge.to.clone());
            }
        }

        // 3. 最后找任意匹配边
        for edge in &edges {
            if !edge.fallback && (edge.condition.is_none() || edge.condition.as_ref().is_some_and(|c| c(state))) {
                return Ok(edge.to.clone());
            }
        }

        Err(GraphError::Terminal(TerminalError::Unrouted {
            node: current.to_string(),
            attempted_conditions: Vec::new(),
        }))
    }

    /// 统一跳转校验 — 验证边存在并记录访问计数。
    ///
    /// 返回 `Result<EdgeTransitionResult, GraphError>`：
    /// - `Err` = 边不存在（MissingEdge）
    /// - `Ok(EdgeTransitionResult)` = 边存在，访问计数结果
    fn transition(
        graph: &Graph,
        current: &str,
        target: &str,
        edge_visits: &mut EdgeVisits,
    ) -> Result<EdgeTransitionResult, GraphError> {
        let edge = graph.find_edge(current, target).ok_or_else(|| {
            GraphError::Terminal(TerminalError::MissingEdge {
                from: current.to_string(),
                to: target.to_string(),
            })
        })?;

        let result = edge_visits.record(current, target, edge.policy.as_ref());
        Ok(result)
    }

    /// 查找下一个节点。
    ///
    /// 优先级：
    /// 1. 匹配 condition 的非 fallback 边
    /// 2. 无条件非 fallback 边
    /// 3. 匹配 condition 的 fallback 边
    /// 4. 无条件 fallback 边
    /// 5. 无匹配 → Unrouted TerminalError
    fn find_next_node<'a>(
        graph: &'a Graph,
        current: &str,
        state: &State,
    ) -> Result<(String, Option<&'a EdgePolicy>), GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            return Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            ))));
        }

        // 1. 匹配 condition 的非 fallback 边
        for edge in &edges {
            if !edge.fallback
                && edge.condition.as_ref().is_some_and(|c| c(state))
            {
                return Ok((edge.to.clone(), edge.policy.as_ref()));
            }
        }

        // 2. 无条件非 fallback 边
        for edge in &edges {
            if !edge.fallback && edge.condition.is_none() {
                return Ok((edge.to.clone(), edge.policy.as_ref()));
            }
        }

        // 3. 匹配 condition 的 fallback 边
        for edge in &edges {
            if edge.fallback
                && (edge.condition.is_none() || edge.condition.as_ref().is_some_and(|c| c(state)))
            {
                return Ok((edge.to.clone(), edge.policy.as_ref()));
            }
        }

        // 4. 无条件 fallback 边
        for edge in &edges {
            if edge.fallback && edge.condition.is_none() {
                return Ok((edge.to.clone(), edge.policy.as_ref()));
            }
        }

        // 5. 无匹配 → Unrouted
        let attempted: Vec<crate::error::ConditionEval> = edges
            .iter()
            .map(|e| crate::error::ConditionEval {
                edge: format!("{}→{}", e.from, e.to),
                condition: e.condition.as_ref().map(|_| "condition".to_string()),
                matched: e.condition.as_ref().map_or(false, |c| c(state)),
            })
            .collect();

        Err(GraphError::Terminal(TerminalError::Unrouted {
            node: current.to_string(),
            attempted_conditions: attempted,
        }))
    }
}

// Graph 通过 #[derive(Clone)] 自动实现 Clone。
// EdgeCondition 使用 Arc 包装，支持 Clone。
