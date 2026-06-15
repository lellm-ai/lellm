//! Graph 执行引擎。
//!
//! 提供阻塞执行（`execute`）与流式执行（`execute_stream`）两种模式。

use std::time::Instant;

use crate::error::GraphError;
use crate::event::{GraphEvent, GraphStream};
use crate::graph::Graph;
use crate::node::{GraphNode, NextStep};
use crate::state::{ExecutionEntry, GraphResult, State};

/// Graph 执行器。
pub struct GraphExecutor;

impl GraphExecutor {
    /// 执行 Graph（阻塞模式）。
    pub async fn execute(graph: &Graph, initial_state: State) -> Result<GraphResult, GraphError> {
        let start_time = Instant::now();
        let mut state = initial_state;
        let mut execution_log = Vec::new();

        let mut current = graph.start_node().to_string();

        loop {
            let node = graph
                .nodes
                .get(&current)
                .ok_or_else(|| GraphError::NodeNotFound(current.clone()))?;

            let start = Instant::now();
            let result = node.execute(&mut state).await;
            let end = Instant::now();

            let success = result.is_ok();
            execution_log.push(ExecutionEntry {
                node_name: current.clone(),
                start_time: start,
                end_time: end,
                success,
            });

            match result {
                Ok(next) => match next {
                    NextStep::Goto(target) => {
                        current = target;
                    }
                    NextStep::GoToNext => {
                        if current == graph.end_node() {
                            break;
                        }
                        current = Self::find_next_node(graph, &current)?;
                    }
                    NextStep::End => {
                        break;
                    }
                },
                Err(e) => {
                    tracing::error!(
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
    /// use std::sync::Arc;
    /// let graph = Arc::new(graph_builder.build().unwrap());
    /// let mut stream = GraphExecutor::execute_stream(graph, initial_state);
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
    pub fn execute_stream(graph: std::sync::Arc<Graph>, initial_state: State) -> GraphStream {
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let start_time = Instant::now();
            let mut state = initial_state;
            let mut execution_log = Vec::new();
            let mut current = graph.start_node().to_string();

            let send = |event: GraphEvent| async {
                if tx.send(event).await.is_err() {
                    tracing::warn!("graph event consumer dropped");
                }
            };

            loop {
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
                            current = target;
                        }
                        NextStep::GoToNext => {
                            if current == graph.end_node() {
                                break;
                            }
                            match Self::find_next_node(&graph, &current) {
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
    fn find_next_node(graph: &Graph, current: &str) -> Result<String, GraphError> {
        let edges = graph.edges_from(current);

        if edges.is_empty() {
            return Err(GraphError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            )));
        }

        for edge in &edges {
            if edge.condition.is_none() {
                return Ok(edge.to.clone());
            }
        }

        Err(GraphError::InvalidGraph(format!(
            "node '{}' only has conditional edges but no condition was true",
            current
        )))
    }
}
