//! 测试用执行器 — 替代已删除的 SimpleExecutor。
//!
//! 提供四种执行模式：
//! - `execute()` — 阻塞执行，返回 `GraphResult`
//! - `execute_stream()` — 非持久化流式执行
//! - `execute_stream_with_checkpoint()` — 持久化执行（每节点同步保存检查点）
//! - `execute_stream_with_restore()` — 持久化恢复（从检查点续跑，同一 trace 继续保存）

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::checkpoint::Checkpoint;
use crate::error::{GraphError, TerminalError};
use crate::event::{GraphExecution, GraphHandle};
use crate::exec::CheckpointConfig;
use crate::exec::execution_engine::{ExecutionEngine, ExecutorState, NextAction};
use crate::graph::Graph;
use crate::ids::TraceId;
#[allow(deprecated)]
use crate::node::{BarrierNode, ConditionNode, FlowNode, LeafNode, NodeKind};
use crate::state::workflow_state::WorkflowState;
use crate::state::{ExecutionEntry, GraphResult, State};

// ─── SimpleExecutor 兼容层 ────────────────────────────────────────

/// 兼容 SimpleExecutor 的 API，供测试使用。
///
/// 仅支持 `Graph<State, StateMerge>`（默认泛型参数）。
pub struct SimpleExecutor {
    max_steps: usize,
}

impl Default for SimpleExecutor {
    fn default() -> Self {
        Self { max_steps: 100 }
    }
}

impl SimpleExecutor {
    pub fn new(max_steps: usize) -> Self {
        Self { max_steps }
    }

    pub async fn execute(
        &self,
        graph: Arc<Graph>,
        mut state: State,
    ) -> Result<GraphResult, GraphError> {
        let trace_id = TraceId::new();
        let start_time = Instant::now();
        let mut execution_log: Vec<ExecutionEntry> = Vec::new();

        let cancel = CancellationToken::new();
        // TestExecutor 不需要自动 checkpoint
        let mut engine = ExecutionEngine::new(&mut state, None, cancel, None, None);

        // 执行循环 — 与 run_inline 一致，但记录 ExecutionEntry
        let mut current = graph.start_node().to_string();
        let mut step: usize = 0;

        loop {
            step += 1;
            if step > self.max_steps {
                return Err(GraphError::Terminal(
                    crate::error::TerminalError::StepsExceeded {
                        limit: self.max_steps,
                    },
                ));
            }

            let node = match graph.nodes.get(&current) {
                Some(n) => n,
                None => {
                    return Err(GraphError::Terminal(
                        crate::error::TerminalError::NodeNotFound(current.clone()),
                    ));
                }
            };

            let node_name = current.clone();
            let node_start = Instant::now();

            // 根据 NodeKind 分发执行
            #[allow(deprecated)]
            match node {
                NodeKind::Task(n) => {
                    let mut ctx = engine.build_node_context();
                    n.execute(&mut ctx).await?;
                }
                NodeKind::Condition(n) => {
                    let mut ctx = engine.build_leaf_context();
                    <ConditionNode as LeafNode>::execute(n, &mut ctx).await?;
                }
                NodeKind::Barrier(n) => {
                    let mut ctx = engine.build_leaf_context();
                    <BarrierNode as LeafNode>::execute(n, &mut ctx).await?;
                }
                NodeKind::External(n) => {
                    let mut ctx = engine.build_node_context();
                    n.execute(&mut ctx).await?;
                }
                NodeKind::ExternalLeaf(n) => {
                    let mut ctx = engine.build_leaf_context();
                    n.execute(&mut ctx).await?;
                }
                NodeKind::Parallel(p) => {
                    // ExecutorOperation 直接接收 &mut ExecutionEngine
                    p.execute(&mut engine).await?;
                }
                NodeKind::Subgraph(_subgraph) => {
                    // TODO: 实现 Subgraph 执行
                    // 由 ExecutionEngine 负责 Frame 管理、状态投影、Checkpoint 和恢复
                    tracing::warn!("Subgraph execution not yet implemented");
                }
            }

            let node_duration = node_start.elapsed();

            execution_log.push(ExecutionEntry {
                step,
                node_name,
                start_time: node_start,
                end_time: start_time.checked_add(node_duration).unwrap_or(start_time),
                success: true,
                error: None,
            });

            // commit mutations (Unit of Work) — 对 Parallel 是空操作
            // （replace_state 已经直接替换了状态，mutation buffer 为空）
            engine.commit();

            // 提取控制信号
            let (next_action, _signal) = engine.take_control();

            // 处理路由
            match next_action {
                NextAction::End => break,
                NextAction::Goto(target) => {
                    current = target;
                }
                NextAction::Next => {
                    if current == graph.end_node() {
                        break;
                    }
                    current = graph.resolve_next_inline(&current, engine.state())?;
                }
            }
        }

        let duration = start_time.elapsed();
        let final_state = state;

        Ok(GraphResult {
            trace_id,
            state: final_state,
            execution_log,
            duration,
            trace: None,
        })
    }

    /// 非持久化执行（行为不变，不做图结构校验）。
    pub fn execute_stream(&self, graph: Arc<Graph>, state: State) -> GraphExecution<State> {
        self.spawn(graph, state, TraceId::new(), None, None)
    }

    /// 持久化执行 — 每节点同步保存检查点（新 trace）。
    ///
    /// 入口校验：含 Parallel/Subgraph/Barrier 的图在此拒绝（`RestoreUnsupported`），
    /// 不等崩溃后恢复才报错。
    pub fn execute_stream_with_checkpoint(
        &self,
        graph: Arc<Graph>,
        state: State,
        config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError> {
        graph.validate_persistable()?;
        Ok(self.spawn(graph, state, TraceId::new(), Some(config), None))
    }

    /// 持久化恢复 — 从检查点续跑，并**继续保存**到同一 trace。
    ///
    /// - `trace_id` — 延续原 trace（新检查点保存于该 trace，seq 单调前进）
    /// - `config` — 恢复后继续保存的持久化配置（store/retention）
    ///
    /// # 入口校验（覆盖直接构造的检查点）
    ///
    /// 图结构 / format_version / graph_hash / next_node 存在 / 步数边界 /
    /// **最新性**（`config.store` 存在时：传入检查点必须是该 trace 最新，
    /// 否则 `RestoreNotLatest` — 要恢复旧检查点请换新 trace）。
    pub async fn execute_stream_with_restore(
        &self,
        graph: Arc<Graph>,
        restore_from: Checkpoint<State>,
        trace_id: TraceId,
        config: CheckpointConfig<State>,
    ) -> Result<GraphExecution<State>, GraphError> {
        graph.validate_persistable()?;
        Self::validate_restore_checkpoint(&graph, &restore_from, self.max_steps)?;

        // 最新性检查（需 store I/O → 本方法 async）：
        // 只接受该 trace 的最新检查点并续写原 trace，避免未定义的历史分叉
        if let Some(store) = &config.store {
            match store.load_latest(&trace_id).await {
                Ok(Some(latest)) => {
                    if latest.id != restore_from.checkpoint_id {
                        return Err(GraphError::Terminal(TerminalError::RestoreNotLatest {
                            checkpoint: restore_from.checkpoint_id.to_string(),
                            latest: latest.id.to_string(),
                        }));
                    }
                }
                Ok(None) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!("no checkpoints found for trace {trace_id}"),
                    }));
                }
                Err(e) => {
                    return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                        reason: format!("load latest checkpoint: {e}"),
                    }));
                }
            }
        }

        let state = State::restore(restore_from.state.clone());
        Ok(self.spawn(graph, state, trace_id, Some(config), Some(restore_from)))
    }

    /// 恢复入口同步校验（版本 / 指纹 / 节点存在 / 步数边界）。
    fn validate_restore_checkpoint(
        graph: &Graph,
        cp: &Checkpoint<State>,
        max_steps: usize,
    ) -> Result<(), GraphError> {
        if cp.format_version != crate::checkpoint::CHECKPOINT_FORMAT_VERSION {
            return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                reason: format!(
                    "unsupported checkpoint format_version: {} (expected {})",
                    cp.format_version,
                    crate::checkpoint::CHECKPOINT_FORMAT_VERSION
                ),
            }));
        }
        if cp.graph_hash != graph.canonical_hash() {
            return Err(GraphError::Terminal(TerminalError::RestoreFailed {
                reason: format!(
                    "graph hash mismatch: expected {:016x}, got {:016x}",
                    graph.canonical_hash(),
                    cp.graph_hash
                ),
            }));
        }
        if let Some(n) = &cp.next_node {
            if !graph.node_map().contains_key(&n.0) {
                return Err(GraphError::Terminal(TerminalError::NodeNotFound(
                    n.0.clone(),
                )));
            }
            // 还有下一节点但预算已耗尽 → 执行前报错（完成态除外）
            if cp.steps_used >= max_steps {
                return Err(GraphError::Terminal(TerminalError::StepsExceeded {
                    limit: max_steps,
                }));
            }
        }
        // next_node = None（完成态）：允许预算耗尽（零执行直接返回完成）
        Ok(())
    }

    /// 统一 spawn — 通道 + run_execution_loop。
    fn spawn(
        &self,
        graph: Arc<Graph>,
        state: State,
        trace_id: TraceId,
        checkpoint: Option<CheckpointConfig<State>>,
        restore_from: Option<Checkpoint<State>>,
    ) -> GraphExecution<State> {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
        let (decision_tx, decision_rx) = tokio::sync::mpsc::channel(256);
        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel(1);

        let cancel = CancellationToken::new();
        let handle = GraphHandle::new(decision_tx, cancel_tx);

        tokio::spawn(crate::exec::execution_loop::run_execution_loop(
            graph,
            state,
            self.max_steps,
            trace_id,
            event_tx,
            decision_rx,
            cancel_rx,
            cancel,
            checkpoint,
            None, // trace_sink
            restore_from,
        ));

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }
}
