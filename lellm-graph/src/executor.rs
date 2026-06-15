//! Graph 执行引擎。

use std::time::Instant;

use crate::error::GraphError;
use crate::graph::Graph;
use crate::node::{GraphNode, NextStep};
use crate::state::{ExecutionEntry, GraphResult, State};

/// Graph 执行器。
pub struct GraphExecutor;

impl GraphExecutor {
    /// 执行 Graph。
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
