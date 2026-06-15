//! Graph 和 GraphBuilder。

use indexmap::IndexMap;

use crate::error::GraphError;
use crate::node::NodeKind;
use crate::state::State;

/// 边条件回调类型别名。
pub type EdgeCondition = Box<dyn Fn(&State) -> bool + Send + Sync>;

/// 边（Edge）。
pub struct Edge {
    pub from: String,
    pub to: String,
    pub condition: Option<EdgeCondition>,
}

/// 图（Graph）。
pub struct Graph {
    pub(crate) nodes: IndexMap<String, NodeKind>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) start: String,
    pub(crate) end: String,
}

impl Graph {
    /// 获取节点名称列表。
    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.keys().map(|s| s.as_str()).collect()
    }

    /// 获取起始节点名称。
    pub fn start_node(&self) -> &str {
        &self.start
    }

    /// 获取结束节点名称。
    pub fn end_node(&self) -> &str {
        &self.end
    }

    /// 获取从指定节点出发的边。
    pub fn edges_from(&self, from: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.from == from).collect()
    }

    /// 验证图结构。
    pub fn validate(&self) -> Result<(), GraphError> {
        if !self.nodes.contains_key(&self.start) {
            return Err(GraphError::InvalidGraph(format!(
                "start node '{}' not found",
                self.start
            )));
        }

        if !self.nodes.contains_key(&self.end) {
            return Err(GraphError::InvalidGraph(format!(
                "end node '{}' not found",
                self.end
            )));
        }

        for edge in &self.edges {
            if !self.nodes.contains_key(&edge.from) {
                return Err(GraphError::InvalidGraph(format!(
                    "edge references non-existent source node '{}'",
                    edge.from
                )));
            }
            if !self.nodes.contains_key(&edge.to) {
                return Err(GraphError::InvalidGraph(format!(
                    "edge references non-existent target node '{}'",
                    edge.to
                )));
            }
        }

        self.detect_cycle()?;

        Ok(())
    }

    /// 检测环。
    fn detect_cycle(&self) -> Result<(), GraphError> {
        use std::collections::HashSet;

        fn dfs(
            node: &str,
            graph: &Graph,
            visited: &mut HashSet<String>,
            rec_stack: &mut HashSet<String>,
        ) -> Result<(), GraphError> {
            visited.insert(node.to_string());
            rec_stack.insert(node.to_string());

            for edge in graph.edges_from(node) {
                if !visited.contains(&edge.to) {
                    dfs(&edge.to, graph, visited, rec_stack)?;
                } else if rec_stack.contains(&edge.to) {
                    return Err(GraphError::InvalidGraph(format!(
                        "cycle detected: {} -> {}",
                        node, edge.to
                    )));
                }
            }

            rec_stack.remove(node);
            Ok(())
        }

        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for node_name in self.nodes.keys() {
            if !visited.contains(node_name) {
                dfs(node_name, self, &mut visited, &mut rec_stack)?;
            }
        }

        Ok(())
    }
}

// ─── GraphBuilder ────────────────────────────────────────────

/// Graph 构建器。
pub struct GraphBuilder {
    name: String,
    nodes: IndexMap<String, NodeKind>,
    edges: Vec<Edge>,
    start: Option<String>,
    end: Option<String>,
}

impl GraphBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: IndexMap::new(),
            edges: Vec::new(),
            start: None,
            end: None,
        }
    }

    /// 设置起始节点。
    pub fn start(mut self, node: impl Into<String>) -> Self {
        self.start = Some(node.into());
        self
    }

    /// 设置结束节点。
    pub fn end(mut self, node: impl Into<String>) -> Self {
        self.end = Some(node.into());
        self
    }

    /// 添加节点。
    pub fn node(mut self, name: impl Into<String>, kind: NodeKind) -> Self {
        let name = name.into();
        self.nodes.insert(name, kind);
        self
    }

    /// 添加边（无条件）。
    pub fn edge(mut self, from: impl Into<String>, to: impl Into<String>) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
        });
        self
    }

    /// 添加条件边。
    pub fn edge_if(
        mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&State) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: Some(Box::new(condition)),
        });
        self
    }

    /// 构建 Graph。
    pub fn build(self) -> Result<Graph, GraphError> {
        let start = self
            .start
            .ok_or_else(|| GraphError::InvalidGraph("start node not set".into()))?;

        let end = self
            .end
            .ok_or_else(|| GraphError::InvalidGraph("end node not set".into()))?;

        let graph = Graph {
            nodes: self.nodes,
            edges: self.edges,
            start,
            end,
        };

        graph.validate()?;

        Ok(graph)
    }

    /// 获取图名称。
    pub fn name(&self) -> &str {
        &self.name
    }
}
