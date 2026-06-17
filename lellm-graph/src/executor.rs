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
use crate::error::{GraphError, ObservedError, TerminalError};
use crate::event::{
    BarrierDecision, BarrierDecisionMessage, BarrierId, FlowEvent, GraphEvent, GraphExecution,
    GraphHandle,
};
use crate::graph::Graph;
use crate::node::{FlowNode, NextStep, NodeKind, ParallelErrorStrategy, StreamNodeResult};
use crate::state::{
    ExecutionEntry, GraphResult, ReducerRegistry, SpanId, State, StateDelta, TraceId,
};

use lellm_runtime::checkpoint::{Checkpoint, CheckpointPolicy, CheckpointStore, CheckpointTrigger};

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

// ─── StepOutcome ──────────────────────────────────────────────

/// 节点执行后的下一步操作。
#[derive(Debug)]
enum StepOutcome {
    /// 继续执行，跳转到指定节点
    Continue(String),
    /// 正常结束（到达 end 节点），由外层发送 GraphComplete
    Break,
    /// 错误已发送（GraphError），直接返回
    ErrorSent,
}

// ─── GraphExecutor ────────────────────────────────────────────

/// Graph 执行器 — 可配置运行时参数。
///
/// 支持可选的 Checkpoint 集成，实现持久化执行。
pub struct GraphExecutor {
    /// 全局运行时步数限制。
    /// 1 Step = 1 Node Entry。
    pub max_steps: usize,
    /// 可选的 Checkpoint 存储后端。
    store: Option<std::sync::Arc<dyn CheckpointStore>>,
    /// Checkpoint 保存频率策略。
    policy: CheckpointPolicy,
    /// 图结构指纹（用于恢复时校验）。
    graph_hash: String,
    /// 待注册的 Reducer（在 run_loop 中应用到 ReducerRegistry）。
    pending_reducers: Vec<(String, lellm_runtime::Reducer)>,
}

impl Clone for GraphExecutor {
    fn clone(&self) -> Self {
        Self {
            max_steps: self.max_steps,
            store: self.store.clone(),
            policy: self.policy.clone(),
            graph_hash: self.graph_hash.clone(),
            pending_reducers: self.pending_reducers.clone(),
        }
    }
}

impl std::fmt::Debug for GraphExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphExecutor")
            .field("max_steps", &self.max_steps)
            .field("has_store", &self.store.is_some())
            .field("policy", &self.policy)
            .field("graph_hash", &self.graph_hash)
            .finish()
    }
}

impl Default for GraphExecutor {
    fn default() -> Self {
        Self {
            max_steps: 50,
            store: None,
            policy: CheckpointPolicy::default(),
            graph_hash: String::new(),
            pending_reducers: Vec::new(),
        }
    }
}

impl GraphExecutor {
    /// 创建基础执行器（无 Checkpoint）。
    pub fn new(max_steps: usize) -> Self {
        Self {
            max_steps,
            store: None,
            policy: CheckpointPolicy::default(),
            graph_hash: String::new(),
            pending_reducers: Vec::new(),
        }
    }

    /// 创建带 Checkpoint 的执行器。
    pub fn with_checkpoint(
        max_steps: usize,
        store: std::sync::Arc<dyn CheckpointStore>,
        policy: CheckpointPolicy,
        graph: &Graph,
    ) -> Self {
        Self {
            max_steps,
            store: Some(store),
            policy,
            graph_hash: graph.hash(),
            pending_reducers: Vec::new(),
        }
    }

    /// 设置 Checkpoint 存储后端。
    pub fn set_store(&mut self, store: std::sync::Arc<dyn CheckpointStore>) {
        self.store = Some(store);
    }

    /// 设置 Checkpoint 频率策略。
    pub fn set_policy(&mut self, policy: CheckpointPolicy) {
        self.policy = policy;
    }

    /// 注册 key 的 Reducer（用于 ParallelNode 合并策略）。
    pub fn register_reducer(&mut self, key: &str, reducer: lellm_runtime::Reducer) {
        // ReducerRegistry 在 run_loop 中创建，这里存储待注册的 reducers
        // 通过一个新字段传递
        self.pending_reducers.push((key.to_string(), reducer));
    }

    /// 设置图结构指纹。
    pub fn set_graph(&mut self, graph: &Graph) {
        self.graph_hash = graph.hash();
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
        let (decision_tx, decision_rx) = mpsc::channel(16);
        let (cancel_tx, cancel_rx) = mpsc::channel(1);
        let (checkpoint_tx, checkpoint_rx) = mpsc::channel(8);

        let handle = GraphHandle::new(decision_tx, cancel_tx, checkpoint_tx);

        tokio::spawn(async move {
            executor
                .run_loop(
                    graph,
                    initial_state,
                    event_tx,
                    decision_rx,
                    cancel_rx,
                    checkpoint_rx,
                )
                .await;
        });

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }

    /// 主执行循环。
    async fn run_loop(
        &self,
        graph: std::sync::Arc<Graph>,
        initial_state: State,
        event_tx: mpsc::Sender<GraphEvent>,
        mut decision_rx: mpsc::Receiver<BarrierDecisionMessage>,
        mut cancel_rx: mpsc::Receiver<()>,
        mut checkpoint_rx: mpsc::Receiver<()>,
    ) {
        let start_time = Instant::now();
        let mut state = initial_state;
        let mut execution_log = Vec::new();
        let mut decision_registry = DecisionRegistry::new();
        let mut reducer_registry = ReducerRegistry::new();

        // 应用待注册的 Reducer
        for (key, reducer) in &self.pending_reducers {
            reducer_registry.register(key, *reducer);
        }

        let mut current = graph.start_node().to_string();
        let mut step: usize = 0;
        let trace_id = TraceId::default();

        // 发射 GraphStart 事件
        if self
            .send(&event_tx, GraphEvent::GraphStart { trace_id })
            .await
        {
            return;
        }

        loop {
            // ⚡ 取消信号检测
            if cancel_rx.try_recv().is_ok() {
                self.send_graph_error(
                    &event_tx,
                    GraphError::Terminal(TerminalError::BarrierCancelled {
                        node: "execution cancelled by handle".into(),
                    }),
                    &state,
                    &execution_log,
                    start_time,
                    trace_id,
                )
                .await;
                return;
            }

            // ⚡ Manual checkpoint 信号检测 — 立即保存
            if checkpoint_rx.try_recv().is_ok() {
                self.save_checkpoint_if_needed(
                    &event_tx,
                    &trace_id,
                    &current,
                    &state,
                    step,
                    CheckpointTrigger::Explicit,
                )
                .await;
            }

            step += 1;

            // ⚡ 运行时熔断
            if step > self.max_steps {
                self.send_graph_error(
                    &event_tx,
                    GraphError::Terminal(TerminalError::StepsExceeded {
                        limit: self.max_steps,
                    }),
                    &state,
                    &execution_log,
                    start_time,
                    trace_id,
                )
                .await;
                return;
            }

            // 查找节点
            let node = match graph.nodes.get(&current) {
                Some(n) => n,
                None => {
                    self.send_graph_error(
                        &event_tx,
                        GraphError::Terminal(TerminalError::NodeNotFound(current.clone())),
                        &state,
                        &execution_log,
                        start_time,
                        trace_id,
                    )
                    .await;
                    return;
                }
            };

            let node_name = current.clone();
            let span_id = SpanId::new();

            if self
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
            let result = if matches!(node, NodeKind::Parallel(_)) {
                self.handle_parallel(node, &state, &event_tx, span_id, &node_name)
                    .await
            } else {
                node.execute_stream(&state, &event_tx, span_id).await
            };
            let node_end = Instant::now();
            let duration = node_end.duration_since(node_start);

            match result {
                Ok(StreamNodeResult::Continue {
                    deltas,
                    next,
                    span_id,
                    observed,
                }) => {
                    // Apply deltas to state
                    if matches!(node, NodeKind::Parallel(_)) {
                        // Parallel 节点 — 使用 merge_deltas 处理多 writer 冲突
                        if let Err(e) = reducer_registry.merge_deltas(&mut state, &deltas) {
                            // 冲突即错误 — 终止执行
                            self.handle_error(
                                &event_tx,
                                &mut execution_log,
                                &node_name,
                                node_start,
                                node_end,
                                span_id,
                                step,
                                trace_id,
                                GraphError::Terminal(TerminalError::StateError(format!(
                                    "parallel merge conflict: {}",
                                    e
                                ))),
                                &state,
                            )
                            .await;
                            return;
                        }
                        // 发射 StateChanged 事件
                        for delta in &deltas {
                            let _ = self
                                .send(
                                    &event_tx,
                                    GraphEvent::Node {
                                        span_id: SpanId::new(),
                                        node_name: node_name.to_string(),
                                        event: FlowEvent::StateChanged {
                                            node_id: node_name.to_string(),
                                            delta: delta.clone(),
                                        },
                                    },
                                )
                                .await;
                        }
                    } else {
                        self.apply_deltas(
                            &event_tx,
                            &mut reducer_registry,
                            &mut state,
                            &node_name,
                            &deltas,
                        )
                        .await;
                    }

                    let outcome = self
                        .handle_continue(
                            &event_tx,
                            &graph,
                            &current,
                            &mut state,
                            &mut execution_log,
                            next,
                            span_id,
                            observed,
                            step,
                            &node_name,
                            node_start,
                            node_end,
                            duration,
                            trace_id,
                        )
                        .await;

                    match outcome {
                        StepOutcome::Continue(target) => {
                            // 💾 Checkpoint: Explicit 模式下保存（节点标注了 .checkpoint()）
                            self.save_checkpoint_if_needed(
                                &event_tx,
                                &trace_id,
                                &target,
                                &state,
                                step,
                                CheckpointTrigger::Explicit,
                            )
                            .await;
                            current = target;
                        }
                        StepOutcome::Break => {
                            // 正常结束（到达 end 节点）
                            self.send_graph_complete(
                                &event_tx,
                                &state,
                                &execution_log,
                                start_time,
                                trace_id,
                            )
                            .await;
                            return;
                        }
                        StepOutcome::ErrorSent => {
                            return;
                        }
                    }
                }

                Ok(StreamNodeResult::Pause {
                    deltas: barrier_deltas,
                    node_name: barrier_name,
                    span_id,
                    timeout,
                    default_action,
                    ..
                }) => {
                    // Apply pre-pause deltas
                    self.apply_deltas(
                        &event_tx,
                        &mut reducer_registry,
                        &mut state,
                        &barrier_name,
                        &barrier_deltas,
                    )
                    .await;

                    let outcome = self
                        .handle_barrier(
                            &event_tx,
                            &graph,
                            &mut decision_rx,
                            &mut decision_registry,
                            &mut cancel_rx,
                            &mut reducer_registry,
                            node,
                            &current,
                            &mut state,
                            &mut execution_log,
                            &barrier_name,
                            span_id,
                            timeout,
                            default_action,
                            step,
                            node_start,
                            trace_id,
                        )
                        .await;

                    match outcome {
                        StepOutcome::Continue(target) => {
                            // 💾 Checkpoint: BarrierResolved 模式下保存
                            self.save_checkpoint_if_needed(
                                &event_tx,
                                &trace_id,
                                &target,
                                &state,
                                step,
                                CheckpointTrigger::BarrierResolved,
                            )
                            .await;
                            current = target;
                        }
                        StepOutcome::Break => {
                            // 正常结束（到达 end 节点）
                            self.send_graph_complete(
                                &event_tx,
                                &state,
                                &execution_log,
                                start_time,
                                trace_id,
                            )
                            .await;
                            return;
                        }
                        StepOutcome::ErrorSent => {
                            return;
                        }
                    }
                }

                Ok(StreamNodeResult::Fallback {
                    deltas: fallback_deltas,
                    reason,
                    node_name: fallback_node,
                }) => {
                    // Apply pre-fallback deltas
                    self.apply_deltas(
                        &event_tx,
                        &mut reducer_registry,
                        &mut state,
                        &fallback_node,
                        &fallback_deltas,
                    )
                    .await;

                    let outcome = self
                        .handle_fallback(
                            &event_tx,
                            &graph,
                            &current,
                            &mut state,
                            &mut execution_log,
                            &fallback_node,
                            &reason,
                            step,
                            node_start,
                            node_end,
                            trace_id,
                        )
                        .await;

                    match outcome {
                        StepOutcome::Continue(target) => {
                            current = target;
                        }
                        StepOutcome::ErrorSent => {
                            return;
                        }
                        StepOutcome::Break => {
                            // handle_fallback 不会返回 Break
                            unreachable!("handle_fallback only returns Continue or ErrorSent");
                        }
                    }
                }

                Err(e) => {
                    self.handle_error(
                        &event_tx,
                        &mut execution_log,
                        &node_name,
                        node_start,
                        node_end,
                        span_id,
                        step,
                        trace_id,
                        e,
                        &state,
                    )
                    .await;
                    return;
                }
            }
        }
    }

    /// 处理节点正常完成（`StreamNodeResult::Continue`）。
    ///
    /// 发送 NodeEnd 事件，记录执行日志，解析下一步路由。
    async fn handle_continue(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        graph: &Graph,
        current: &str,
        state: &mut State,
        execution_log: &mut Vec<ExecutionEntry>,
        next: NextStep,
        span_id: SpanId,
        observed: Option<ObservedError>,
        step: usize,
        node_name: &str,
        node_start: Instant,
        node_end: Instant,
        duration: std::time::Duration,
        trace_id: TraceId,
    ) -> StepOutcome {
        // 记录执行日志
        execution_log.push(ExecutionEntry {
            step,
            node_name: node_name.to_string(),
            start_time: node_start,
            end_time: node_end,
            success: true,
        });

        // 发送 NodeEnd 事件
        if self
            .send(
                event_tx,
                GraphEvent::NodeEnd {
                    node_name: node_name.to_string(),
                    trace_id,
                    span_id,
                    success: true,
                    duration,
                },
            )
            .await
        {
            return StepOutcome::Break;
        }

        // 如果有观测错误，发送 ObservedError 事件
        if let Some(error) = observed {
            if self
                .send(
                    event_tx,
                    GraphEvent::ObservedError {
                        error,
                        node_name: node_name.to_string(),
                    },
                )
                .await
            {
                return StepOutcome::Break;
            }
        }

        // 🛑 end 节点检查
        if current == graph.end_node() {
            return StepOutcome::Break;
        }

        // 解析下一步路由
        match self.resolve_next(graph, current, state, next) {
            Ok(target) => StepOutcome::Continue(target),
            Err(e) => {
                self.send_graph_error(event_tx, e, state, execution_log, Instant::now(), trace_id)
                    .await;
                StepOutcome::ErrorSent
            }
        }
    }

    /// 处理 Barrier 暂停（`StreamNodeResult::Pause`）。
    ///
    /// 发射 BarrierWaiting 事件，等待外部决策，应用决策结果。
    async fn handle_barrier(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        graph: &Graph,
        decision_rx: &mut mpsc::Receiver<BarrierDecisionMessage>,
        decision_registry: &mut DecisionRegistry,
        cancel_rx: &mut mpsc::Receiver<()>,
        reducer_registry: &mut ReducerRegistry,
        node: &NodeKind,
        current: &str,
        state: &mut State,
        execution_log: &mut Vec<ExecutionEntry>,
        barrier_name: &str,
        span_id: SpanId,
        timeout: Option<std::time::Duration>,
        default_action: BarrierDefaultAction,
        step: usize,
        node_start: Instant,
        trace_id: TraceId,
    ) -> StepOutcome {
        let barrier_id = decision_registry.next_id(barrier_name);

        // 发射 BarrierWaiting 事件
        if self
            .send(
                event_tx,
                GraphEvent::BarrierWaiting {
                    barrier_id: barrier_id.clone(),
                    node_name: barrier_name.to_string(),
                    span_id,
                },
            )
            .await
        {
            return StepOutcome::Break;
        }

        // 等待决策
        let decision = self
            .wait_barrier_decision(
                decision_rx,
                decision_registry,
                &barrier_id,
                timeout,
                &default_action,
                cancel_rx,
            )
            .await;

        // 检查取消信号
        if cancel_rx.try_recv().is_ok() {
            self.send_graph_error(
                event_tx,
                GraphError::Terminal(TerminalError::BarrierCancelled {
                    node: barrier_name.to_string(),
                }),
                state,
                execution_log,
                node_start,
                trace_id,
            )
            .await;
            return StepOutcome::ErrorSent;
        }

        // 发射 BarrierResolved 事件
        if self
            .send(
                event_tx,
                GraphEvent::BarrierResolved {
                    barrier_id: barrier_id.clone(),
                    decision: decision.clone(),
                },
            )
            .await
        {
            return StepOutcome::Break;
        }

        // 应用决策 — apply_decision 返回 (NextStep, Vec<StateDelta>)
        let (next, barrier_deltas) = match node {
            NodeKind::Barrier(b) => b.apply_decision(decision),
            _ => {
                self.send_graph_error(
                    event_tx,
                    GraphError::Terminal(TerminalError::InvalidGraph(
                        "expected BarrierNode but got unexpected node type for BarrierPaused"
                            .to_string(),
                    )),
                    state,
                    execution_log,
                    node_start,
                    trace_id,
                )
                .await;
                return StepOutcome::ErrorSent;
            }
        };

        // Apply decision deltas
        self.apply_deltas(
            event_tx,
            reducer_registry,
            state,
            barrier_name,
            &barrier_deltas,
        )
        .await;

        // 记录执行日志
        let end_time = Instant::now();
        execution_log.push(ExecutionEntry {
            step,
            node_name: barrier_name.to_string(),
            start_time: node_start,
            end_time,
            success: true,
        });

        // 发送 NodeEnd 事件
        if self
            .send(
                event_tx,
                GraphEvent::NodeEnd {
                    node_name: barrier_name.to_string(),
                    trace_id,
                    span_id,
                    success: true,
                    duration: end_time.duration_since(node_start),
                },
            )
            .await
        {
            return StepOutcome::Break;
        }

        // 🛑 end 节点检查
        if current == graph.end_node() {
            return StepOutcome::Break;
        }

        // 解析下一步路由
        match self.resolve_next(graph, current, state, next) {
            Ok(target) => StepOutcome::Continue(target),
            Err(e) => {
                self.send_graph_error(event_tx, e, state, execution_log, end_time, trace_id)
                    .await;
                StepOutcome::ErrorSent
            }
        }
    }

    // ─── handle_fallback ──────────────────────────────────────

    /// 处理节点 Fallback（`StreamNodeResult::Fallback`）。
    ///
    /// Fallback 是控制流 — 节点主动声明降级策略。
    async fn handle_fallback(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        graph: &Graph,
        current: &str,
        state: &mut State,
        execution_log: &mut Vec<ExecutionEntry>,
        fallback_node: &str,
        reason: &str,
        step: usize,
        node_start: Instant,
        node_end: Instant,
        trace_id: TraceId,
    ) -> StepOutcome {
        // 记录执行日志
        execution_log.push(ExecutionEntry {
            step,
            node_name: fallback_node.to_string(),
            start_time: node_start,
            end_time: node_end,
            success: false,
        });

        // 查找 fallback 边
        if let Some(fallback_target) = graph.find_fallback_edge(current) {
            // 发送降级 ObservedError 事件
            if self
                .send(
                    event_tx,
                    GraphEvent::ObservedError {
                        error: ObservedError::Degraded {
                            node: fallback_node.to_string(),
                            message: format!("fallback to '{}': {}", fallback_target, reason),
                        },
                        node_name: fallback_node.to_string(),
                    },
                )
                .await
            {
                return StepOutcome::ErrorSent;
            }
            StepOutcome::Continue(fallback_target)
        } else {
            // 无 fallback 边 → 终止
            self.send_graph_error(
                event_tx,
                GraphError::Terminal(TerminalError::NodeExecutionFailed {
                    node: fallback_node.to_string(),
                    source: format!("fallback with no fallback edge: {}", reason).into(),
                }),
                state,
                execution_log,
                node_end,
                trace_id,
            )
            .await;
            StepOutcome::ErrorSent
        }
    }

    // ─── handle_error ─────────────────────────────────────────

    /// 处理节点执行错误。
    async fn handle_error(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        execution_log: &mut Vec<ExecutionEntry>,
        node_name: &str,
        node_start: Instant,
        node_end: Instant,
        span_id: SpanId,
        step: usize,
        trace_id: TraceId,
        error: GraphError,
        state: &State,
    ) {
        let duration = node_end.duration_since(node_start);

        // 记录执行日志
        execution_log.push(ExecutionEntry {
            step,
            node_name: node_name.to_string(),
            start_time: node_start,
            end_time: node_end,
            success: false,
        });

        // 发送 NodeEnd (failure) 事件
        if self
            .send(
                event_tx,
                GraphEvent::NodeEnd {
                    node_name: node_name.to_string(),
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

        // 发送 GraphError 事件
        self.send_graph_error(event_tx, error, state, execution_log, node_end, trace_id)
            .await;
    }

    // ─── handle_parallel ──────────────────────────────────────

    /// 处理并行节点（`NodeKind::Parallel`）。
    ///
    /// Fork State 快照给每个分支，并发执行，收集所有 Delta 后合并。
    async fn handle_parallel(
        &self,
        node: &NodeKind,
        state: &State,
        event_tx: &mpsc::Sender<GraphEvent>,
        parent_span_id: SpanId,
        node_name: &str,
    ) -> Result<StreamNodeResult, GraphError> {
        let parallel = match node {
            NodeKind::Parallel(p) => p,
            _ => unreachable!("handle_parallel called on non-Parallel node"),
        };

        let branch_count = parallel.branch_count();
        let error_strategy = parallel.error_strategy();
        let display_name = parallel.label().unwrap_or(node_name).to_string();

        // 发射 ParallelStarted 事件
        if self
            .send(
                event_tx,
                GraphEvent::Node {
                    span_id: parent_span_id,
                    node_name: node_name.to_string(),
                    event: FlowEvent::ParallelStarted {
                        node_id: display_name.clone(),
                        branch_count,
                        span_id: parent_span_id,
                    },
                },
            )
            .await
        {
            return Err(GraphError::Terminal(TerminalError::InvalidGraph(
                "consumer dropped during parallel execution".into(),
            )));
        }

        let parallel_start = Instant::now();

        // Fork State 快照，spawn 所有分支
        let mut handles = Vec::with_capacity(branch_count);
        for (branch_name, branch_node) in parallel.branches_iter() {
            let state_copy = state.clone();
            let branch_node = branch_node.clone();
            let name = branch_name.to_string();

            let handle = tokio::spawn(async move {
                let branch_start = Instant::now();
                // 分支直接调用 execute（阻塞模式），不经过 stream
                // 因为 stream 会发射重复的 NodeStart/NodeEnd 事件
                let result = branch_node.execute(&state_copy).await;
                let branch_end = Instant::now();
                (name, result, branch_end.duration_since(branch_start))
            });

            handles.push(handle);
        }

        // 收集所有结果
        let mut all_deltas: Vec<lellm_runtime::StateDelta> = Vec::new();
        let mut first_error: Option<GraphError> = None;
        let mut any_failure = false;

        for handle in handles {
            let (branch_name, result, branch_duration) = match handle.await {
                Ok(res) => res,
                Err(join_err) => {
                    let err = GraphError::Terminal(TerminalError::NodeExecutionFailed {
                        node: format!("{}/{}", display_name, "<unknown>"),
                        source: join_err.into(),
                    });
                    // 发射 BranchCompleted (failure)
                    let _ = self
                        .send(
                            event_tx,
                            GraphEvent::Node {
                                span_id: parent_span_id,
                                node_name: node_name.to_string(),
                                event: FlowEvent::BranchCompleted {
                                    branch_name: "<unknown>".to_string(),
                                    node_id: display_name.clone(),
                                    span_id: SpanId::new(),
                                    success: false,
                                    duration: std::time::Duration::ZERO,
                                },
                            },
                        )
                        .await;

                    if matches!(error_strategy, ParallelErrorStrategy::FailFast) {
                        return Err(err);
                    }
                    first_error.get_or_insert(err);
                    any_failure = true;
                    continue;
                }
            };

            match result {
                Ok(output) => {
                    all_deltas.extend(output.deltas);

                    // 发射 BranchCompleted (success)
                    let _ = self
                        .send(
                            event_tx,
                            GraphEvent::Node {
                                span_id: parent_span_id,
                                node_name: node_name.to_string(),
                                event: FlowEvent::BranchCompleted {
                                    branch_name: branch_name.clone(),
                                    node_id: display_name.clone(),
                                    span_id: SpanId::new(),
                                    success: true,
                                    duration: branch_duration,
                                },
                            },
                        )
                        .await;
                }
                Err(e) => {
                    // 发射 BranchCompleted (failure)
                    let _ = self
                        .send(
                            event_tx,
                            GraphEvent::Node {
                                span_id: parent_span_id,
                                node_name: node_name.to_string(),
                                event: FlowEvent::BranchCompleted {
                                    branch_name: branch_name.clone(),
                                    node_id: display_name.clone(),
                                    span_id: SpanId::new(),
                                    success: false,
                                    duration: branch_duration,
                                },
                            },
                        )
                        .await;

                    if matches!(error_strategy, ParallelErrorStrategy::FailFast) {
                        return Err(e);
                    }
                    first_error.get_or_insert(e);
                    any_failure = true;
                }
            }
        }

        // 如果有失败且为 CollectAll 模式，返回错误
        if any_failure {
            return Err(first_error.unwrap());
        }

        // 合并所有 Delta — 使用 merge_deltas 处理多 writer 冲突
        // 注意：此处不直接 apply，返回给外层统一 apply
        // merge_deltas 用于验证冲突，实际 apply 由外层 handle_continue 完成

        // 发射 ParallelCompleted 事件
        let parallel_duration = parallel_start.elapsed();
        let _ = self
            .send(
                event_tx,
                GraphEvent::Node {
                    span_id: parent_span_id,
                    node_name: node_name.to_string(),
                    event: FlowEvent::ParallelCompleted {
                        node_id: display_name,
                        span_id: parent_span_id,
                        duration: parallel_duration,
                    },
                },
            )
            .await;

        Ok(StreamNodeResult::Continue {
            deltas: all_deltas,
            next: NextStep::GoToNext,
            span_id: parent_span_id,
            observed: None,
        })
    }

    // ─── 辅助方法 ──────────────────────────────────────────────

    /// 应用节点返回的 StateDelta 到 State，并发射 StateChanged 事件。
    async fn apply_deltas(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        reducer_registry: &mut ReducerRegistry,
        state: &mut State,
        node_name: &str,
        deltas: &[StateDelta],
    ) {
        for delta in deltas {
            // Apply delta to state
            if let Err(e) = reducer_registry.apply_delta(state, delta) {
                tracing::warn!(
                    node = %node_name,
                    key = %delta.key,
                    error = %e,
                    "failed to apply state delta"
                );
            }

            // 发射 StateChanged 事件
            let _ = self
                .send(
                    event_tx,
                    GraphEvent::Node {
                        span_id: SpanId::new(), // TODO: 使用节点的 span_id
                        node_name: node_name.to_string(),
                        event: FlowEvent::StateChanged {
                            node_id: node_name.to_string(),
                            delta: delta.clone(),
                        },
                    },
                )
                .await;
        }
    }

    /// 发送事件，返回 `true` 表示 consumer 已断开（应终止执行）。
    async fn send(&self, event_tx: &mpsc::Sender<GraphEvent>, event: GraphEvent) -> bool {
        event_tx.send(event).await.is_err()
    }

    /// 发送 GraphError 事件（携带 state 快照）。
    async fn send_graph_error(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        error: GraphError,
        state: &State,
        _execution_log: &Vec<ExecutionEntry>,
        _start_time: Instant,
        _trace_id: TraceId,
    ) {
        let _ = self
            .send(
                event_tx,
                GraphEvent::GraphError {
                    error,
                    state: state.clone(),
                },
            )
            .await;
    }

    /// 发送 GraphComplete 事件。
    async fn send_graph_complete(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        state: &State,
        execution_log: &[ExecutionEntry],
        start_time: Instant,
        trace_id: TraceId,
    ) {
        // 💾 Checkpoint: ExecutionCompleted 模式下保存最终状态
        if self.policy.should_checkpoint_on_completion() {
            if let Some(store) = &self.store {
                let ck = Checkpoint::new(trace_id, &self.graph_hash, "__complete__", state.clone());
                match store.save(&ck).await {
                    Ok(()) => {
                        let _ = self
                            .send(
                                event_tx,
                                GraphEvent::CheckpointSaved {
                                    checkpoint_id: ck.checkpoint_id.clone(),
                                    node_name: "__complete__".to_string(),
                                    step: execution_log.len(),
                                },
                            )
                            .await;
                        tracing::debug!(
                            checkpoint = %ck.checkpoint_id,
                            "final checkpoint saved on completion"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "final checkpoint save failed");
                    }
                }
            }
        }

        let _ = self
            .send(
                event_tx,
                GraphEvent::GraphComplete {
                    result: GraphResult {
                        trace_id,
                        state: state.clone(),
                        execution_log: execution_log.to_vec(),
                        duration: start_time.elapsed(),
                    },
                },
            )
            .await;
    }

    // ─── 等待 Barrier 决策 ─────────────────────────────────────

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

    // ─── Checkpoint 方法 ──────────────────────────────────────

    /// 根据策略判断是否需要保存 Checkpoint，如需则保存。
    ///
    /// - `EveryNode` — 每次调用都保存
    /// - `BarrierOnly` — 仅在 Barrier 分支调用时保存
    /// - `Manual` — 仅在 `manual_pending` 为 true 时保存
    async fn save_checkpoint_if_needed(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        trace_id: &TraceId,
        next_node: &str,
        state: &State,
        step: usize,
        trigger: CheckpointTrigger,
    ) {
        // 检查该 trigger 是否启用
        let should_save = match trigger {
            CheckpointTrigger::BarrierResolved => self.policy.should_checkpoint_on_barrier(),
            CheckpointTrigger::ExecutionCompleted => self.policy.should_checkpoint_on_completion(),
            CheckpointTrigger::HumanDecision => self.policy.should_checkpoint_on_human_decision(),
            CheckpointTrigger::Explicit => self.policy.should_checkpoint_on_explicit(),
            CheckpointTrigger::Adaptive => false, // v0.4
        };

        if !should_save {
            return;
        }

        let store = match &self.store {
            Some(s) => s,
            None => return,
        };

        let ck = Checkpoint::new(*trace_id, &self.graph_hash, next_node, state.clone());

        match store.save(&ck).await {
            Ok(()) => {
                let _ = self
                    .send(
                        event_tx,
                        GraphEvent::CheckpointSaved {
                            checkpoint_id: ck.checkpoint_id.clone(),
                            node_name: next_node.to_string(),
                            step,
                        },
                    )
                    .await;
                tracing::debug!(
                    checkpoint = %ck.checkpoint_id,
                    node = %next_node,
                    step,
                    "checkpoint saved"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, node = %next_node, step, "checkpoint save failed");
            }
        }
    }

    /// 从 Checkpoint 恢复执行。
    ///
    /// 1. 加载 trace_id 对应的最新 Checkpoint
    /// 2. 校验 `graph_hash`（Strict 模式）
    /// 3. 从 `current_node` 继续执行
    ///
    /// # 参数
    /// - `store` — Checkpoint 存储后端
    /// - `trace_id` — 要恢复的执行 Trace ID
    /// - `graph` — 当前图定义（用于 hash 校验）
    ///
    /// # 错误
    /// - Checkpoint 不存在 → `NotFound`
    /// - 图结构已变更 → `InvalidGraph`（Strict 模式）
    pub async fn resume_from(
        &self,
        store: &dyn CheckpointStore,
        trace_id: &lellm_runtime::TraceId,
        graph: &std::sync::Arc<Graph>,
    ) -> Result<GraphExecution, GraphError> {
        // 加载最新 Checkpoint
        let checkpoint = store
            .load_latest(trace_id)
            .await
            .map_err(|e| {
                GraphError::Terminal(TerminalError::InvalidGraph(format!(
                    "failed to load checkpoint: {}",
                    e
                )))
            })?
            .ok_or_else(|| {
                GraphError::Terminal(TerminalError::InvalidGraph(format!(
                    "no checkpoint found for trace {}",
                    trace_id
                )))
            })?;

        // 校验图结构指纹
        let current_hash = graph.hash();
        if checkpoint.graph_hash != current_hash {
            tracing::warn!(
                saved_hash = %checkpoint.graph_hash,
                current_hash = %current_hash,
                "graph structure has changed since checkpoint — resuming anyway (Force mode)",
            );
            // v0.4 暂不实现 Strict 拒绝，仅 warn
            // TODO(v0.4): 支持 GraphHashMode::Strict 拒绝恢复
        }

        // 构建带 Checkpoint 的执行器
        let executor = Self {
            max_steps: self.max_steps,
            store: self.store.clone(),
            policy: self.policy.clone(),
            graph_hash: current_hash,
            pending_reducers: self.pending_reducers.clone(),
        };

        // 从 Checkpoint 的 current_node 继续
        let initial_state = checkpoint.state.clone();

        // 覆盖 graph start — 从 checkpoint 的节点继续
        // 注意：run_loop 会从 graph.start_node() 开始，但我们需要从 current_node 开始
        // 所以这里需要特殊处理
        let execution = executor.execute_stream(graph.clone(), initial_state);

        // ⚠️ 限制：resume_from 目前从 start_node 重新开始，
        // 但携带 checkpoint 的 state。完整 resume（从 current_node 继续）
        // 需要 run_loop 支持自定义起始节点，v0.4 后续迭代实现。
        Ok(execution)
    }
}
