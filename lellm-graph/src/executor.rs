//! Graph 执行引擎。
//!
//! 提供阻塞执行（`execute`）与流式执行（`execute_stream`）两种模式。
//! 运行时全局步数限制（`max_steps`）防止无限循环。

use std::time::Instant;

use crate::error::GraphError;
use crate::event::{GraphEvent, GraphStream};
use crate::graph::Graph;
use crate::node::{GraphNode, NextStep};
use crate::state::{ExecutionEntry, GraphResult, State};

/// Graph 执行器 — 可配置运行时参数。
///
/// ```rust,ignore
/// // 使用默认配置（max_steps = 50）
/// let result = GraphExecutor::default()
///     .execute(&graph, initial_state)
///     .await?;
///
/// // 自定义步数限制
/// let executor = GraphExecutor::new(100);
/// let result = executor.execute(&graph, initial_state).await?;
/// ```
#[derive(Clone, Debug)]
pub struct GraphExecutor {
    /// 全局运行时步数限制（单次图执行允许经历的最大节点执行总数）。
    ///
    /// 这是防止大模型在条件边上陷入死循环的绝对安全防御线。
    pub max_steps: usize,
}

impl Default for GraphExecutor {
    fn default() -> Self {
        Self { max_steps: 50 }
    }
}

impl GraphExecutor {
    /// 创建 GraphExecutor，指定最大步数。
    pub fn new(max_steps: usize) -> Self {
        Self { max_steps }
    }

    /// 执行 Graph（阻塞模式）。
    pub async fn execute(
        &self,
        graph: &Graph,
        initial_state: State,
    ) -> Result<GraphResult, GraphError> {
        let start_time = Instant::now();
        let mut state = initial_state;
        let mut execution_log = Vec::new();

        let mut current = graph.start_node().to_string();
        let mut step: usize = 0;

        loop {
            step += 1;

            // ⚡ 运行时熔断 — 防止无限循环
            if step > self.max_steps {
                tracing::error!(
                    max_steps = %self.max_steps,
                    current_node = %current,
                    "Graph execution halted: step limit exceeded (potential infinite loop)"
                );
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

            let success = result.is_ok();
            execution_log.push(ExecutionEntry {
                node_name: current.clone(),
                start_time: node_start,
                end_time: node_end,
                success,
            });

            match result {
                Ok(next) => match next {
                    NextStep::Goto(target) => {
                        tracing::debug!(
                            step = step,
                            from = %current,
                            to = %target,
                            "Goto (explicit jump, may form cycle)"
                        );
                        current = target;
                    }
                    NextStep::GoToNext => {
                        if current == graph.end_node() {
                            break;
                        }
                        current = Self::find_next_node(graph, &current, &state)?;
                    }
                    NextStep::End => {
                        break;
                    }
                },
                Err(e) => {
                    tracing::error!(
                        step = step,
                        node = %current,
                        error = %e,
                        "graph execution failed"
                    );
                    return Err(e);
                }
            }
        }

        let duration = start_time.elapsed();

        Ok(GraphResult {
            state,
            execution_log,
            duration,
        })
    }

    /// 流式执行 Graph，返回事件接收器。
    ///
    /// 消费者通过 `GraphStream` 实时接收执行事件：
    /// - `NodeStart` / `NodeEnd` — 节点执行边界
    /// - `Agent` — AgentNode 内部的 AgentEvent（Provider 事件、工具调用等）
    /// - `GraphComplete` — 执行完成（含最终 State）
    /// - `GraphError` — 执行出错
    ///
    /// ```rust,ignore
    /// let executor = GraphExecutor::default();
    /// let graph = std::sync::Arc::new(graph_builder.build().unwrap());
    /// let mut stream = executor.execute_stream(graph, initial_state);
    /// while let Some(event) = stream.recv().await {
    ///     match event {
    ///         GraphEvent::Agent { node_name, event } => {
    ///             // 处理 Agent 内部事件
    ///         }
    ///         GraphEvent::GraphComplete { result } => {
    ///             // 获取最终状态
    ///         }
    ///         _ => {}
    ///     }
    /// }
    /// ```
    pub fn execute_stream(
        &self,
        graph: std::sync::Arc<Graph>,
        initial_state: State,
    ) -> GraphStream {
        let executor = self.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let start_time = Instant::now();
            let mut state = initial_state;
            let mut execution_log = Vec::new();
            let mut current = graph.start_node().to_string();
            let mut step: usize = 0;

            let send = |event: GraphEvent| async {
                if tx.send(event).await.is_err() {
                    tracing::warn!("graph event consumer dropped");
                }
            };

            loop {
                step += 1;

                // ⚡ 运行时熔断
                if step > executor.max_steps {
                    let err = GraphError::StepsExceeded {
                        limit: executor.max_steps,
                    };
                    tracing::error!(
                        step = step,
                        max_steps = %executor.max_steps,
                        "Graph execution halted: step limit exceeded (potential infinite loop)"
                    );
                    let _ = send(GraphEvent::GraphError { error: err }).await;
                    break;
                }

                let node = match graph.nodes.get(&current) {
                    Some(n) => n,
                    None => {
                        let err = GraphError::NodeNotFound(current.clone());
                        let _ = send(GraphEvent::GraphError { error: err }).await;
                        break;
                    }
                };

                let node_name = current.clone();
                let _ = send(GraphEvent::NodeStart {
                    node_name: node_name.clone(),
                })
                .await;

                let node_start = Instant::now();
                let result = node.execute_stream(&mut state, &tx).await;
                let node_end = Instant::now();
                let duration = node_end.duration_since(node_start);
                let success = result.is_ok();

                execution_log.push(ExecutionEntry {
                    node_name: node_name.clone(),
                    start_time: node_start,
                    end_time: node_end,
                    success,
                });

                let _ = send(GraphEvent::NodeEnd {
                    node_name: node_name.clone(),
                    success,
                    duration,
                })
                .await;

                match result {
                    Ok(next) => match next {
                        NextStep::Goto(target) => {
                            tracing::debug!(
                                step = step,
                                from = %current,
                                to = %target,
                                "Goto (explicit jump, may form cycle)"
                            );
                            current = target;
                        }
                        NextStep::GoToNext => {
                            if current == graph.end_node() {
                                break;
                            }
                            match Self::find_next_node(&graph, &current, &state) {
                                Ok(next_node) => current = next_node,
                                Err(e) => {
                                    let _ = send(GraphEvent::GraphError { error: e }).await;
                                    break;
                                }
                            }
                        }
                        NextStep::End => {
                            break;
                        }
                    },
                    Err(e) => {
                        tracing::error!(
                            step = step,
                            node = %current,
                            error = %e,
                            "graph execution failed"
                        );
                        let _ = send(GraphEvent::GraphError { error: e }).await;
                        break;
                    }
                }
            }

            // Success path - send complete event
            let duration = start_time.elapsed();
            let result = GraphResult {
                state,
                execution_log,
                duration,
            };
            let _ = send(GraphEvent::GraphComplete { result }).await;
        });

        rx
    }

    /// 查找下一个节点。
    ///
    /// 优先级：
    /// 1. 条件边（按声明顺序评估，返回第一个条件为 true 的目标）
    /// 2. 无条件边（默认流转）
    /// 3. 若无出边，返回错误
    fn find_next_node(graph: &Graph, current: &str, state: &State) -> Result<String, GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            return Err(GraphError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            )));
        }

        // 先评估条件边
        for edge in &edges {
            if let Some(ref condition) = edge.condition
                && condition(state)
            {
                return Ok(edge.to.clone());
            }
        }

        // 无条件边（默认流转）
        for edge in &edges {
            if edge.condition.is_none() {
                return Ok(edge.to.clone());
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
