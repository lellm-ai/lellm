//! 测试用执行器 — 替代已删除的 SimpleExecutor。
//!
//! 提供两种执行模式：
//! - `execute()` — 阻塞执行，返回 `GraphResult`
//! - `execute_stream()` — 流式执行，返回 `GraphExecution { stream, handle }`

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::error::GraphError;
use crate::event::{GraphExecution, GraphHandle};
use crate::exec::execution_engine::{ExecutionEngine, ExecutorState, NextAction};
use crate::graph::Graph;
use crate::ids::TraceId;
use crate::node::{BarrierNode, ConditionNode, FlowNode, LeafNode, NodeKind};
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

    pub fn execute_stream(&self, graph: Arc<Graph>, state: State) -> GraphExecution<State> {
        self.execute_stream_with_restore(graph, state, None)
    }

    pub fn execute_stream_with_restore(
        &self,
        graph: Arc<Graph>,
        state: State,
        restore_from: Option<crate::checkpoint::Checkpoint<State>>,
    ) -> GraphExecution<State> {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
        let (decision_tx, decision_rx) = tokio::sync::mpsc::channel(256);
        let (cancel_tx, cancel_rx) = tokio::sync::mpsc::channel(1);

        let trace_id = TraceId::new();
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
            None, // checkpoint
            None, // trace_sink
            restore_from,
        ));

        GraphExecution {
            stream: event_rx,
            handle,
        }
    }
}
