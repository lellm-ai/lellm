//! Graph 执行引擎。
//!
//! v04: 使用 BranchState + NodeContext，Control Plane (RuntimeEvent) / Data Plane (StreamChunk) 分离。
//! 运行时全局步数限制（`max_steps`）防止无限循环。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::barrier_node::BarrierDefaultAction;
use crate::branch_state::BranchState;
use crate::checkpoint::{
    Checkpoint, CheckpointPolicy, CheckpointScore, CheckpointStore, CheckpointTrigger,
    ExecutionMetadata, IncrementalSnapshotState,
};
use crate::delta::ReducerRegistry;
use crate::error::{GraphError, TerminalError};
use crate::event::{
    BarrierDecision, BarrierDecisionMessage, BarrierId, FlowEvent, GraphEvent, GraphExecution,
    GraphHandle,
};
use crate::graph::Graph;
use crate::ids::{SpanId, TraceId};
use crate::node::{FlowNode, NodeKind};
use crate::node_context::{ExecutionSignal, NextAction, NodeContext, NodeMetadata};
use crate::runtime_event::RuntimeEvent;
use crate::state::{ExecutionEntry, GraphResult, State, StateEffect};
use crate::stream_emitter::StreamEmitter;
use crate::workflow_state::WorkflowState;

// ─── DecisionRegistry ─────────────────────────────────────────

#[allow(dead_code)]
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

    #[allow(dead_code)]
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

#[derive(Debug)]
enum StepOutcome {
    Continue(String),
    Break,
    ErrorSent,
}

// ─── GraphExecutor ────────────────────────────────────────────

/// Graph 执行器 — v04: BranchState + NodeContext。
pub struct GraphExecutor {
    pub max_steps: usize,
    store: Option<Arc<dyn CheckpointStore>>,
    policy: CheckpointPolicy,
    graph_hash: String,
    pending_reducers: Vec<(String, crate::delta::Reducer)>,
    checkpoint_score: CheckpointScore,
    last_checkpoint_state: Option<State>,
    delta_compact_threshold: usize,
}

impl Clone for GraphExecutor {
    fn clone(&self) -> Self {
        Self {
            max_steps: self.max_steps,
            store: self.store.clone(),
            policy: self.policy.clone(),
            graph_hash: self.graph_hash.clone(),
            pending_reducers: self.pending_reducers.clone(),
            checkpoint_score: self.checkpoint_score.clone(),
            last_checkpoint_state: self.last_checkpoint_state.clone(),
            delta_compact_threshold: self.delta_compact_threshold,
        }
    }
}

impl std::fmt::Debug for GraphExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphExecutor")
            .field("max_steps", &self.max_steps)
            .field("has_store", &self.store.is_some())
            .field("policy", &self.policy)
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
            checkpoint_score: CheckpointScore::default(),
            last_checkpoint_state: None,
            delta_compact_threshold: 20,
        }
    }
}

impl GraphExecutor {
    pub fn new(max_steps: usize) -> Self {
        Self {
            max_steps,
            ..Default::default()
        }
    }

    pub fn with_checkpoint(
        max_steps: usize,
        store: Arc<dyn CheckpointStore>,
        policy: CheckpointPolicy,
        graph: &Graph,
    ) -> Self {
        Self {
            max_steps,
            store: Some(store),
            policy,
            graph_hash: graph.hash(),
            ..Default::default()
        }
    }

    pub fn register_reducer(&mut self, key: &str, reducer: crate::delta::Reducer) {
        self.pending_reducers.push((key.to_string(), reducer));
    }

    pub fn set_graph(&mut self, graph: &Graph) {
        self.graph_hash = graph.hash();
    }

    // ─── 阻塞执行 ──────────────────────────────────────────────

    pub async fn execute(
        &self,
        graph: Arc<Graph>,
        initial_state: State,
    ) -> Result<GraphResult, GraphError> {
        let GraphExecution { mut stream, handle } = self.execute_stream(graph, initial_state);
        drop(handle);

        let mut result = None;
        while let Some(event) = stream.recv().await {
            match event {
                GraphEvent::GraphComplete { result: r } => result = Some(Ok(r)),
                GraphEvent::GraphError { error, .. } => result = Some(Err(error)),
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

    pub fn execute_stream(&self, graph: Arc<Graph>, initial_state: State) -> GraphExecution {
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
                    None,
                    None,
                )
                .await;
        });

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }

    /// 内部：流式执行，支持指定起始节点和追踪 ID。
    ///
    /// - `start_node`: 覆盖图的起始节点（用于 Checkpoint 恢复）
    /// - `trace_id`: 覆盖追踪 ID（用于 Checkpoint 恢复）
    fn execute_stream_with(
        &self,
        graph: Arc<Graph>,
        initial_state: State,
        start_node: Option<String>,
        trace_id: Option<TraceId>,
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
                    start_node,
                    trace_id,
                )
                .await;
        });

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }

    // ─── 主执行循环 ────────────────────────────────────────────

    async fn run_loop(
        &self,
        graph: Arc<Graph>,
        initial_state: State,
        event_tx: mpsc::Sender<GraphEvent>,
        mut decision_rx: mpsc::Receiver<BarrierDecisionMessage>,
        mut cancel_rx: mpsc::Receiver<()>,
        mut checkpoint_rx: mpsc::Receiver<()>,
        start_node: Option<String>,
        trace_id: Option<TraceId>,
    ) {
        let start_time = Instant::now();
        let mut state = initial_state;
        let mut execution_log = Vec::new();
        let mut decision_registry = DecisionRegistry::new();
        let mut _reducer_registry = ReducerRegistry::new();
        let mut snapshot_state = IncrementalSnapshotState::new(self.delta_compact_threshold);

        for (key, reducer) in &self.pending_reducers {
            _reducer_registry.register(key, *reducer);
        }

        let mut current = start_node.unwrap_or_else(|| graph.start_node().to_string());
        let mut step: usize = 0;
        let trace_id = trace_id.unwrap_or_default();

        // 发射 GraphStart (RuntimeEvent) + GraphEvent
        self.emit_runtime(
            &event_tx,
            RuntimeEvent::ExecutionStarted {
                trace_id,
                graph_name: graph.name().to_string(),
            },
        )
        .await;
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
                    start_time,
                    trace_id,
                )
                .await;
                return;
            }

            // ⚡ Manual checkpoint 信号检测
            if checkpoint_rx.try_recv().is_ok() {
                self.save_checkpoint_if_needed(
                    &event_tx,
                    &trace_id,
                    &current,
                    &state,
                    step,
                    CheckpointTrigger::Explicit,
                    &mut snapshot_state,
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
                        start_time,
                        trace_id,
                    )
                    .await;
                    return;
                }
            };

            let node_name = current.clone();
            let span_id = SpanId::new();

            // 发射 NodeStarted (RuntimeEvent) + NodeStart (GraphEvent)
            self.emit_runtime(
                &event_tx,
                RuntimeEvent::NodeStarted {
                    node_name: node_name.clone(),
                    trace_id,
                    span_id,
                    step,
                },
            )
            .await;
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

            // ── 核心：使用 BranchState + NodeContext 执行节点 ──
            let exec_result = self
                .execute_node(node, &mut state, &node_name, span_id)
                .await;
            let node_end = Instant::now();
            let duration = node_end.duration_since(node_start);

            match exec_result {
                Ok((next_action, signal, metadata, flow_events)) => {
                    // 记录执行日志
                    execution_log.push(ExecutionEntry {
                        step,
                        node_name: node_name.clone(),
                        start_time: node_start,
                        end_time: node_end,
                        success: true,
                        error: None,
                    });

                    // 发射节点产生的 FlowEvent（如 ParallelStarted, BranchCompleted 等）
                    for flow_event in flow_events {
                        if self
                            .send(
                                &event_tx,
                                GraphEvent::Node {
                                    span_id,
                                    node_name: node_name.clone(),
                                    event: flow_event,
                                },
                            )
                            .await
                        {
                            return;
                        }
                    }

                    // Adaptive Checkpoint
                    if self.policy.has_adaptive_trigger() {
                        let exec_metadata = ExecutionMetadata {
                            duration_ms: duration.as_millis() as u64,
                            token_cost: metadata.token_cost,
                            has_side_effects: metadata.has_side_effects,
                        };
                        self.save_checkpoint_if_needed(
                            &event_tx,
                            &trace_id,
                            &current,
                            &state,
                            step,
                            CheckpointTrigger::Adaptive(exec_metadata),
                            &mut snapshot_state,
                        )
                        .await;
                    }

                    // 发射 NodeCompleted (RuntimeEvent) + NodeEnd (GraphEvent)
                    self.emit_runtime(
                        &event_tx,
                        RuntimeEvent::NodeCompleted {
                            node_name: node_name.clone(),
                            trace_id,
                            span_id,
                            duration,
                        },
                    )
                    .await;
                    if self
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

                    // 处理 ExecutionSignal
                    if let Some(signal) = signal {
                        match signal {
                            ExecutionSignal::Pause {
                                barrier_id,
                                timeout,
                            } => {
                                let outcome = self
                                    .handle_barrier_signal(
                                        &event_tx,
                                        &graph,
                                        &mut decision_rx,
                                        &mut decision_registry,
                                        &mut cancel_rx,
                                        node,
                                        &current,
                                        &mut state,
                                        &mut execution_log,
                                        &node_name,
                                        barrier_id,
                                        timeout,
                                        step,
                                        node_start,
                                        trace_id,
                                    )
                                    .await;
                                match outcome {
                                    StepOutcome::Continue(target) => {
                                        self.save_checkpoint_if_needed(
                                            &event_tx,
                                            &trace_id,
                                            &target,
                                            &state,
                                            step,
                                            CheckpointTrigger::BarrierResolved,
                                            &mut snapshot_state,
                                        )
                                        .await;
                                        current = target;
                                    }
                                    StepOutcome::Break => {
                                        self.send_graph_complete(
                                            &event_tx,
                                            &state,
                                            &execution_log,
                                            start_time,
                                            trace_id,
                                            &mut snapshot_state,
                                        )
                                        .await;
                                        return;
                                    }
                                    StepOutcome::ErrorSent => return,
                                }
                                continue;
                            }
                        }
                    }

                    // 处理 NextAction
                    let outcome = match next_action {
                        NextAction::End => StepOutcome::Break,
                        NextAction::Goto(target) => StepOutcome::Continue(target),
                        NextAction::Next => {
                            // 🛑 end 节点检查
                            if current == graph.end_node() {
                                StepOutcome::Break
                            } else {
                                match self.resolve_next(&graph, &current, &state) {
                                    Ok(target) => StepOutcome::Continue(target),
                                    Err(e) => {
                                        self.send_graph_error(
                                            &event_tx, e, &state, start_time, trace_id,
                                        )
                                        .await;
                                        StepOutcome::ErrorSent
                                    }
                                }
                            }
                        }
                    };

                    match outcome {
                        StepOutcome::Continue(target) => {
                            self.save_checkpoint_if_needed(
                                &event_tx,
                                &trace_id,
                                &target,
                                &state,
                                step,
                                CheckpointTrigger::Explicit,
                                &mut snapshot_state,
                            )
                            .await;
                            current = target;
                        }
                        StepOutcome::Break => {
                            self.send_graph_complete(
                                &event_tx,
                                &state,
                                &execution_log,
                                start_time,
                                trace_id,
                                &mut snapshot_state,
                            )
                            .await;
                            return;
                        }
                        StepOutcome::ErrorSent => return,
                    }
                }
                Err(e) => {
                    // 记录失败日志
                    let error_str = e.to_string();
                    execution_log.push(ExecutionEntry {
                        step,
                        node_name: node_name.clone(),
                        start_time: node_start,
                        end_time: node_end,
                        success: false,
                        error: Some(error_str.clone()),
                    });

                    self.emit_runtime(
                        &event_tx,
                        RuntimeEvent::NodeFailed {
                            node_name: node_name.clone(),
                            trace_id,
                            span_id,
                            error: e.to_string(),
                        },
                    )
                    .await;
                    if self
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

                    self.send_graph_error(&event_tx, e, &state, start_time, trace_id)
                        .await;
                    return;
                }
            }
        }
    }

    // ─── 节点执行（核心）──────────────────────────────────────

    /// 使用 BranchState + NodeContext 执行单个节点。
    ///
    /// 返回 (NextAction, Option<ExecutionSignal>, NodeMetadata, Vec<FlowEvent>)。
    async fn execute_node(
        &self,
        node: &NodeKind,
        state: &mut State,
        _node_name: &str,
        _span_id: SpanId,
    ) -> Result<
        (
            NextAction,
            Option<ExecutionSignal>,
            NodeMetadata,
            Vec<FlowEvent>,
        ),
        GraphError,
    > {
        // 1. 创建 BranchState（从当前 State）
        let mut branch = BranchState::from_state(state.clone());

        // 2. 创建 StreamEmitter（数据面通道）
        let (tx, _rx) = mpsc::channel(64);
        let emitter = StreamEmitter::new(tx);

        // 3. 创建 NodeContext（typed state + branch state）
        let mut ctx = NodeContext::new(state, &mut branch, Some(&emitter));

        // 4. 执行节点
        node.execute(&mut ctx).await?;

        // 5. 提取控制信号 + 消费 Effects
        let effects = ctx.consume_effects();
        let (next_action, signal) = ctx.take_control();
        let metadata = ctx.take_metadata();
        let flow_events = ctx.take_flow_events();

        // 5b. 消费 Effects → apply 到 typed state
        for v in effects {
            let effect: <State as crate::workflow_state::WorkflowState>::Effect =
                match serde_json::from_value(v) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
            state.apply(effect);
        }

        Ok((next_action, signal, metadata, flow_events))
    }

    // ─── Barrier 处理 ──────────────────────────────────────────

    async fn handle_barrier_signal(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        graph: &Graph,
        decision_rx: &mut mpsc::Receiver<BarrierDecisionMessage>,
        decision_registry: &mut DecisionRegistry,
        cancel_rx: &mut mpsc::Receiver<()>,
        node: &NodeKind,
        current: &str,
        state: &mut State,
        execution_log: &mut Vec<ExecutionEntry>,
        barrier_name: &str,
        barrier_id: BarrierId,
        timeout: Option<std::time::Duration>,
        step: usize,
        node_start: Instant,
        trace_id: TraceId,
    ) -> StepOutcome {
        // 发射 BarrierWaiting
        self.emit_runtime(
            event_tx,
            RuntimeEvent::BarrierWaiting {
                barrier_id: barrier_id.clone(),
                node_name: barrier_name.to_string(),
                span_id: SpanId::new(),
            },
        )
        .await;
        if self
            .send(
                event_tx,
                GraphEvent::BarrierWaiting {
                    barrier_id: barrier_id.clone(),
                    node_name: barrier_name.to_string(),
                    span_id: SpanId::new(),
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
                cancel_rx,
            )
            .await;

        if cancel_rx.try_recv().is_ok() {
            self.send_graph_error(
                event_tx,
                GraphError::Terminal(TerminalError::BarrierCancelled {
                    node: barrier_name.to_string(),
                }),
                state,
                node_start,
                trace_id,
            )
            .await;
            return StepOutcome::ErrorSent;
        }

        // 发射 BarrierResolved
        self.emit_runtime(
            event_tx,
            RuntimeEvent::BarrierResolved {
                barrier_id: barrier_id.clone(),
            },
        )
        .await;
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

        // 应用决策
        match node {
            NodeKind::Barrier(b) => {
                let mut branch = BranchState::from_state(state.clone());
                let mut ctx = NodeContext::new(state, &mut branch, None);
                b.apply_decision_to_ctx(&mut ctx, decision);
                let (next, _signal) = ctx.take_control();

                // 消费 Effects → apply 到 typed state
                let effects = ctx.consume_effects();
                for v in effects {
                    if let Ok(effect) = serde_json::from_value::<StateEffect>(v) {
                        state.apply(effect);
                    }
                }

                // 记录日志
                let end_time = Instant::now();
                execution_log.push(ExecutionEntry {
                    step,
                    node_name: barrier_name.to_string(),
                    start_time: node_start,
                    end_time,
                    success: true,
                    error: None,
                });

                if self
                    .send(
                        event_tx,
                        GraphEvent::NodeEnd {
                            node_name: barrier_name.to_string(),
                            trace_id,
                            span_id: SpanId::new(),
                            success: true,
                            duration: end_time.duration_since(node_start),
                        },
                    )
                    .await
                {
                    return StepOutcome::Break;
                }

                if current == graph.end_node() {
                    return StepOutcome::Break;
                }

                match next {
                    NextAction::End => StepOutcome::Break,
                    NextAction::Goto(target) => StepOutcome::Continue(target),
                    NextAction::Next => match self.resolve_next(graph, current, state) {
                        Ok(target) => StepOutcome::Continue(target),
                        Err(e) => {
                            self.send_graph_error(event_tx, e, state, end_time, trace_id)
                                .await;
                            StepOutcome::ErrorSent
                        }
                    },
                }
            }
            _ => {
                self.send_graph_error(
                    event_tx,
                    GraphError::Terminal(TerminalError::InvalidGraph(
                        "expected BarrierNode for pause signal".into(),
                    )),
                    state,
                    node_start,
                    trace_id,
                )
                .await;
                StepOutcome::ErrorSent
            }
        }
    }

    // ─── 辅助方法 ──────────────────────────────────────────────

    async fn emit_runtime(
        &self,
        _event_tx: &mpsc::Sender<GraphEvent>,
        runtime_event: RuntimeEvent,
    ) {
        tracing::debug!(?runtime_event, "runtime event");
    }

    async fn send(&self, event_tx: &mpsc::Sender<GraphEvent>, event: GraphEvent) -> bool {
        event_tx.send(event).await.is_err()
    }

    async fn send_graph_error(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        error: GraphError,
        state: &State,
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

    async fn send_graph_complete(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        state: &State,
        execution_log: &[ExecutionEntry],
        start_time: Instant,
        trace_id: TraceId,
        snapshot_state: &mut IncrementalSnapshotState,
    ) {
        self.emit_runtime(
            event_tx,
            RuntimeEvent::ExecutionCompleted {
                trace_id,
                duration: start_time.elapsed(),
            },
        )
        .await;

        if self.policy.should_checkpoint_on_completion() {
            if let Some(store) = &self.store {
                let (base, deltas, current) = snapshot_state.snapshot(state);
                let ck = if let Some(base_state) = base {
                    Checkpoint::with_snapshot(
                        trace_id,
                        &self.graph_hash,
                        "__complete__",
                        current,
                        base_state,
                        deltas,
                    )
                } else {
                    Checkpoint::new(trace_id, &self.graph_hash, "__complete__", state.clone())
                };
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
                    }
                    Err(e) => tracing::warn!(error = %e, "final checkpoint save failed"),
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

    async fn wait_barrier_decision(
        &self,
        decision_rx: &mut mpsc::Receiver<BarrierDecisionMessage>,
        registry: &mut DecisionRegistry,
        target_id: &BarrierId,
        timeout: Option<std::time::Duration>,
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
            return BarrierDecision::Reject {
                reason: "cancelled".into(),
            };
        }

        let default_action = BarrierDefaultAction::Reject;

        if let Some(timeout) = timeout {
            let start = Instant::now();
            loop {
                match tokio::time::timeout(std::time::Duration::from_millis(50), decision_rx.recv())
                    .await
                {
                    Ok(Some(msg)) => {
                        if let Some(decision) = registry.process_message(msg, target_id) {
                            return decision;
                        }
                    }
                    Ok(None) => return Self::default_decision(&default_action),
                    Err(_) => {}
                }
                if cancel_rx.try_recv().is_ok() {
                    return Self::default_decision(&default_action);
                }
                if start.elapsed() >= timeout {
                    return Self::default_decision(&default_action);
                }
            }
        } else {
            loop {
                if let Some(msg) = decision_rx.recv().await {
                    if let Some(decision) = registry.process_message(msg, target_id) {
                        return decision;
                    }
                } else {
                    return Self::default_decision(&default_action);
                }
                if cancel_rx.try_recv().is_ok() {
                    return Self::default_decision(&default_action);
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

    fn resolve_next(
        &self,
        graph: &Graph,
        current: &str,
        state: &State,
    ) -> Result<String, GraphError> {
        Self::find_next_node(graph, current, state)
    }

    fn find_next_node(graph: &Graph, current: &str, state: &State) -> Result<String, GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            return Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            ))));
        }

        // 1. 条件边
        for edge in &edges {
            if edge.is_conditional() && edge.condition.as_ref().is_some_and(|c| c(state)) {
                return Ok(edge.to.clone());
            }
        }

        // 2. 普通边
        for edge in &edges {
            if edge.is_normal() {
                return Ok(edge.to.clone());
            }
        }

        // 3. Fallback 边
        for edge in &edges {
            if edge.fallback {
                return Ok(edge.to.clone());
            }
        }

        // 4. 无匹配
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

    // ─── Checkpoint ────────────────────────────────────────────

    async fn save_checkpoint_if_needed(
        &self,
        event_tx: &mpsc::Sender<GraphEvent>,
        trace_id: &TraceId,
        next_node: &str,
        state: &State,
        step: usize,
        trigger: CheckpointTrigger,
        snapshot_state: &mut IncrementalSnapshotState,
    ) {
        let should_save = match &trigger {
            CheckpointTrigger::BarrierResolved => self.policy.should_checkpoint_on_barrier(),
            CheckpointTrigger::ExecutionCompleted => self.policy.should_checkpoint_on_completion(),
            CheckpointTrigger::HumanDecision => self.policy.should_checkpoint_on_human_decision(),
            CheckpointTrigger::Explicit => self.policy.should_checkpoint_on_explicit(),
            CheckpointTrigger::Adaptive(metadata) => {
                self.checkpoint_score.should_checkpoint(metadata)
            }
        };

        if !should_save {
            return;
        }
        let store = match &self.store {
            Some(s) => s,
            None => return,
        };

        let (base, deltas, current) = snapshot_state.snapshot(state);
        let ck = if let Some(base_state) = base {
            Checkpoint::with_snapshot(
                *trace_id,
                &self.graph_hash,
                next_node,
                current,
                base_state,
                deltas,
            )
        } else {
            Checkpoint::new(*trace_id, &self.graph_hash, next_node, state.clone())
        };

        match store.save(&ck).await {
            Ok(()) => {
                snapshot_state.base_state = Some(state.clone());
                snapshot_state.clear_pending();
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
            }
            Err(e) => tracing::warn!(error = %e, "checkpoint save failed"),
        }
    }

    pub async fn resume_from(
        &self,
        store: &dyn CheckpointStore,
        trace_id: &TraceId,
        graph: &Arc<Graph>,
    ) -> Result<GraphExecution, GraphError> {
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

        let initial_state = checkpoint.state.clone();

        // 解析恢复节点：如果 current_node 是图的合法节点则从中恢复，
        // 否则从 start_node 开始（如 Checkpoint 记录的是 "__complete__"）。
        let resume_node = {
            let cn = checkpoint.current_node.0.as_str();
            if cn == "__complete__" || cn == graph.end_node() {
                // 图已完成或停在终点 — 从起点重新开始
                tracing::warn!(
                    trace_id = %trace_id,
                    current_node = %cn,
                    "checkpoint indicates graph was already complete; \
                     resuming from start node. \
                     Consider using an intermediate checkpoint for true recovery."
                );
                None
            } else if graph.nodes.contains_key(cn) {
                tracing::info!(
                    trace_id = %trace_id,
                    resume_node = %cn,
                    "resuming from checkpoint node"
                );
                Some(cn.to_string())
            } else {
                tracing::warn!(
                    trace_id = %trace_id,
                    current_node = %cn,
                    "checkpoint node not found in graph; resuming from start node"
                );
                None
            }
        };

        // 从 Checkpoint 记录的 current_node 恢复执行（如果有效）
        let execution =
            self.execute_stream_with(graph.clone(), initial_state, resume_node, Some(*trace_id));
        Ok(execution)
    }
}
