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
    /// 边级循环预算 — 限制该边最多被 traversed 的次数。
    /// 为回跳边设置合理的访问上限，防止无限循环。
    pub max_visits: Option<usize>,
}

/// 图（Graph）— 允许有环，循环保护由 GraphExecutor::max_steps 运行时熔断提供。
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

    /// 查找从 `from` 到 `to` 的边。
    pub fn find_edge(&self, from: &str, to: &str) -> Option<&Edge> {
        self.edges.iter().find(|e| e.from == from && e.to == to)
    }

    /// 验证图结构（节点、边引用有效性）。
    ///
    /// 注意：不检测环 — 有环图是合法的，循环保护由 GraphExecutor::max_steps 提供。
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

        Ok(())
    }

    /// 分析图中所有环，生成诊断信息。
    ///
    /// 不阻止构建，仅用于调试和审查。返回结构化分析结果。
    pub fn analyze_cycles(&self) -> CycleAnalysis {
        // Find all elementary cycles
        let mut cycles = Vec::new();
        let mut path = Vec::new();

        // Build adjacency list
        let mut adj: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for edge in &self.edges {
            adj.entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
        }

        // DFS from each node to find cycles
        for node in self.nodes.keys() {
            let mut in_path = std::collections::HashSet::new();
            path.clear();
            self.dfs_cycles(node, node, &adj, &mut in_path, &mut path, &mut cycles);
        }

        // Check which cycles have protection — 检查环中所有边（含闭合边）
        let mut unprotected = cycles
            .iter()
            .filter(|cycle| {
                let has_protection = (0..cycle.len()).any(|i| {
                    let next = (i + 1) % cycle.len();
                    let from = cycle[i].as_str();
                    let to = cycle[next].as_str();
                    self.edges
                        .iter()
                        .any(|e| e.from == from && e.to == to && e.max_visits.is_some())
                });
                !has_protection
            })
            .cloned()
            .collect::<Vec<_>>();
        unprotected.sort();
        unprotected.dedup();

        CycleAnalysis {
            has_cycles: !cycles.is_empty(),
            cycles,
            unprotected_cycles: unprotected,
            total_edges: self.edges.len(),
            protected_edges: self.edges.iter().filter(|e| e.max_visits.is_some()).count(),
        }
    }

    /// DFS 寻找从 start 出发的环。
    fn dfs_cycles(
        &self,
        start: &str,
        current: &str,
        adj: &std::collections::HashMap<String, Vec<String>>,
        in_path: &mut std::collections::HashSet<String>,
        path: &mut Vec<String>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        if in_path.contains(current) {
            return;
        }

        path.push(current.to_string());
        in_path.insert(current.to_string());

        if let Some(neighbors) = adj.get(current) {
            for neighbor in neighbors {
                if neighbor.as_str() == start && path.len() >= 2 {
                    cycles.push(path.clone());
                } else if neighbor.as_str() > start && !in_path.contains(neighbor) {
                    self.dfs_cycles(start, neighbor, adj, in_path, path, cycles);
                }
            }
        }

        path.pop();
        in_path.remove(current);
    }
}

/// 环分析诊断结果。
#[derive(Debug, Clone)]
pub struct CycleAnalysis {
    /// 图是否包含环
    pub has_cycles: bool,
    /// 所有发现的环（每个环是节点路径，不含回到起点的节点）
    pub cycles: Vec<Vec<String>>,
    /// 未受保护的环（缺少 max_visits 的回跳边）
    pub unprotected_cycles: Vec<Vec<String>>,
    /// 总边数
    pub total_edges: usize,
    /// 受保护的边数（设置了 max_visits）
    pub protected_edges: usize,
}

impl CycleAnalysis {
    /// 是否所有环都有边级保护。
    pub fn all_protected(&self) -> bool {
        self.unprotected_cycles.is_empty()
    }

    /// 生成人类可读的诊断报告。
    pub fn report(&self) -> String {
        let mut lines = Vec::new();

        lines.push("=== Graph Cycle Analysis ===".to_string());

        if !self.has_cycles {
            lines.push("No cycles detected — graph is a DAG.".to_string());
            return lines.join("\n");
        }

        lines.push(format!("Found {} cycle(s).", self.cycles.len()));
        lines.push(format!(
            "Edge protection: {}/{} edges have max_visits set.",
            self.protected_edges, self.total_edges
        ));

        for (i, cycle) in self.cycles.iter().enumerate() {
            let cycle_str = cycle.join(" → ");
            lines.push(format!("  Cycle {}: {} → {}", i + 1, cycle_str, cycle[0]));

            if self.unprotected_cycles.contains(cycle) {
                lines.push("    ⚠️ UNPROTECTED — no max_visits on back-edge".to_string());
            } else {
                lines.push("    ✅ Protected by edge-level max_visits".to_string());
            }
        }

        if !self.all_protected() {
            lines.push("".into());
            lines.push(
                "⚠️ Recommendation: Set max_visits on back-edges to prevent infinite loops."
                    .to_string(),
            );
        }

        lines.join("\n")
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
    pub fn start(&mut self, node: impl Into<String>) -> Result<&mut Self, GraphError> {
        self.start = Some(node.into());
        Ok(self)
    }

    /// 设置结束节点。
    pub fn end(&mut self, node: impl Into<String>) -> Result<&mut Self, GraphError> {
        self.end = Some(node.into());
        Ok(self)
    }

    /// 添加节点。
    pub fn node(
        &mut self,
        name: impl Into<String>,
        kind: NodeKind,
    ) -> Result<&mut Self, GraphError> {
        let name = name.into();
        self.nodes.insert(name, kind);
        Ok(self)
    }

    /// 添加边（无条件）。
    pub fn edge(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Result<&mut Self, GraphError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            max_visits: None,
        });
        Ok(self)
    }

    /// 添加条件边。
    pub fn edge_if(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&State) -> bool + Send + Sync + 'static,
    ) -> Result<&mut Self, GraphError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: Some(Box::new(condition)),
            max_visits: None,
        });
        Ok(self)
    }

    /// 添加带访问限制的边（常用于回跳边）。
    pub fn edge_max_visits(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        max_visits: usize,
    ) -> Result<&mut Self, GraphError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            max_visits: Some(max_visits),
        });
        Ok(self)
    }

    /// 添加带访问限制的条件边。
    pub fn edge_if_max_visits(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&State) -> bool + Send + Sync + 'static,
        max_visits: usize,
    ) -> Result<&mut Self, GraphError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: Some(Box::new(condition)),
            max_visits: Some(max_visits),
        });
        Ok(self)
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
