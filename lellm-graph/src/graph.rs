//! Graph 和 GraphBuilder。
//!
//! Edge 三类边模型：
//! - **条件边** (`edge_if`) — `if/else-if` 规则链，按注册顺序求值，first match wins
//! - **普通边** (`edge`) — 无条件非 fallback，条件边无命中时生效
//! - **Fallback 边** (`edge_fallback`) — 最后兜底
//!
//! 运行时安全由 `GraphExecutor::max_steps` 统一负责。

use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::{BuildError, BuildErrors};
use crate::node::NodeKind;
use crate::state::State;

// ─── Edge ──────────────────────────────────────────────────────

/// 边条件回调类型别名。
/// Arc 包装以支持 Graph Clone（条件回调不可 Clone）。
pub type EdgeCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;

/// 边（Edge）— 三类边模型。
///
/// 一个节点的出边分为三类，按固定顺序求值：
/// 1. **条件边** — `condition` 非 None，`fallback` = false。按注册顺序求值，first match wins。
/// 2. **普通边** — `condition` = None，`fallback` = false。条件边无命中时生效。
/// 3. **Fallback 边** — `fallback` = true。最后兜底。
#[derive(Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// 路由条件。Some = 条件边；None = 普通边或 fallback 边。
    pub condition: Option<EdgeCondition>,
    /// 分析用约束（不参与 runtime 决策）
    pub analysis: Option<EdgeAnalysis>,
    /// 是否为 fallback 边（最后兜底）
    pub fallback: bool,
}

impl Edge {
    /// 判断是否为条件边。
    pub fn is_conditional(&self) -> bool {
        self.condition.is_some() && !self.fallback
    }

    /// 判断是否为普通边（无条件非 fallback）。
    pub fn is_normal(&self) -> bool {
        self.condition.is_none() && !self.fallback
    }
}

impl std::fmt::Debug for Edge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edge")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("has_condition", &self.condition.is_some())
            .field("analysis", &self.analysis)
            .field("fallback", &self.fallback)
            .finish()
    }
}

/// 分析用约束 — 仅用于 `analyze_cycles()` 静态分析。
///
/// 不参与执行控制。运行时安全由 `GraphExecutor::max_steps` 负责。
#[derive(Debug, Clone)]
pub struct EdgeAnalysis {
    /// 建议的最大访问次数 — 用于循环分析诊断。
    pub max_visits: Option<usize>,
}

// ─── Graph ─────────────────────────────────────────────────────

/// 图（Graph）— 允许有环，循环保护由 GraphExecutor::max_steps 运行时熔断提供。
#[derive(Clone)]
pub struct Graph {
    pub(crate) name: String,
    pub(crate) nodes: IndexMap<String, NodeKind>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) start: String,
    pub(crate) end: String,
}

impl Graph {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn node_names(&self) -> Vec<&str> {
        self.nodes.keys().map(|s| s.as_str()).collect()
    }

    pub fn start_node(&self) -> &str {
        &self.start
    }

    pub fn end_node(&self) -> &str {
        &self.end
    }

    pub fn edges_from(&self, from: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.from == from).collect()
    }

    pub fn find_edge(&self, from: &str, to: &str) -> Option<&Edge> {
        self.edges.iter().find(|e| e.from == from && e.to == to)
    }

    /// 查找指定节点的 fallback 边目标。
    ///
    /// 用于 RecoverableError 恢复：寻找 fallback 边作为降级路径。
    pub fn find_fallback_edge(&self, from: &str) -> Option<String> {
        self.edges
            .iter()
            .find(|e| e.from == from && e.fallback)
            .map(|e| e.to.clone())
    }

    /// 验证图结构（节点、边引用有效性）。
    ///
    /// 注意：不检测环 — 有环图是合法的，循环保护由 GraphExecutor::max_steps 提供。
    pub fn validate(&self) -> Result<(), crate::error::TerminalError> {
        if !self.nodes.contains_key(&self.start) {
            return Err(crate::error::TerminalError::InvalidGraph(format!(
                "start node '{}' not found",
                self.start
            )));
        }

        if !self.nodes.contains_key(&self.end) {
            return Err(crate::error::TerminalError::InvalidGraph(format!(
                "end node '{}' not found",
                self.end
            )));
        }

        for edge in &self.edges {
            if !self.nodes.contains_key(&edge.from) {
                return Err(crate::error::TerminalError::InvalidGraph(format!(
                    "edge references non-existent source node '{}'",
                    edge.from
                )));
            }
            if !self.nodes.contains_key(&edge.to) {
                return Err(crate::error::TerminalError::InvalidGraph(format!(
                    "edge references non-existent target node '{}'",
                    edge.to
                )));
            }
        }

        Ok(())
    }

    /// 分析图中所有环，生成诊断信息。
    pub fn analyze_cycles(&self) -> CycleAnalysis {
        let mut cycles = Vec::new();
        let mut path = Vec::new();

        let mut adj: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for edge in &self.edges {
            adj.entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
        }

        for node in self.nodes.keys() {
            let mut in_path = std::collections::HashSet::new();
            path.clear();
            self.dfs_cycles(node, node, &adj, &mut in_path, &mut path, &mut cycles);
        }

        // 检查哪些环有 analysis 保护
        let mut unprotected = cycles
            .iter()
            .filter(|cycle| {
                let has_protection = (0..cycle.len()).any(|i| {
                    let next = (i + 1) % cycle.len();
                    let from = cycle[i].as_str();
                    let to = cycle[next].as_str();
                    self.edges.iter().any(|e| {
                        e.from == from
                            && e.to == to
                            && e.analysis.as_ref().is_some_and(|a| a.max_visits.is_some())
                    })
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
            protected_edges: self
                .edges
                .iter()
                .filter(|e| e.analysis.as_ref().is_some_and(|a| a.max_visits.is_some()))
                .count(),
        }
    }

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
    pub has_cycles: bool,
    pub cycles: Vec<Vec<String>>,
    pub unprotected_cycles: Vec<Vec<String>>,
    pub total_edges: usize,
    pub protected_edges: usize,
}

impl CycleAnalysis {
    pub fn all_protected(&self) -> bool {
        self.unprotected_cycles.is_empty()
    }

    pub fn report(&self) -> String {
        let mut lines = Vec::new();
        lines.push("=== Graph Cycle Analysis ===".to_string());

        if !self.has_cycles {
            lines.push("No cycles detected — graph is a DAG.".to_string());
            return lines.join("\n");
        }

        lines.push(format!("Found {} cycle(s).", self.cycles.len()));
        lines.push(format!(
            "Edge protection: {}/{} edges have analysis set.",
            self.protected_edges, self.total_edges
        ));

        for (i, cycle) in self.cycles.iter().enumerate() {
            let cycle_str = cycle.join(" → ");
            lines.push(format!("  Cycle {}: {} → {}", i + 1, cycle_str, cycle[0]));

            if self.unprotected_cycles.contains(cycle) {
                lines.push("    ⚠️ UNPROTECTED — no max_visits on back-edge".into());
            } else {
                lines.push("    ✅ Protected by edge-level analysis".into());
            }
        }

        if !self.all_protected() {
            lines.push("".into());
            lines.push("⚠️ Recommendation: Set analysis.max_visits on back-edges.".to_string());
        }

        lines.join("\n")
    }
}

// ─── PendingEdge ──────────────────────────────────────────────

/// 待完成的边 — 链式调用的中间句柄。
///
/// 由 `GraphBuilder::edge()` / `edge_if()` / `edge_fallback()` 返回。
/// 通过 `.max_visits(n)` 附加循环分析约束。
///
/// ```rust,ignore
/// // 条件回跳 + 循环分析
/// g.edge_if("b", "a", |s| s.should_retry)?.max_visits(5);
///
/// // 普通边 + 循环分析
/// g.edge("b", "a").max_visits(5);
///
/// // 不加分析（直接丢弃 PendingEdge）
/// g.edge("b", "end");
/// ```
pub struct PendingEdge<'a> {
    builder: &'a mut GraphBuilder,
    edge_index: usize,
}

impl<'a> PendingEdge<'a> {
    /// 附加循环分析约束（建议的最大访问次数）。
    ///
    /// 仅用于 `analyze_cycles()` 静态诊断，不参与运行时路由。
    /// 返回 `&mut GraphBuilder` 以便继续链式调用。
    pub fn max_visits(self, n: usize) -> &'a mut GraphBuilder {
        self.builder.edges[self.edge_index].analysis = Some(EdgeAnalysis {
            max_visits: Some(n),
        });
        self.builder
    }
}

// ─── GraphBuilder ─────────────────────────────────────────────

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

    pub fn start(&mut self, node: impl Into<String>) -> &mut Self {
        self.start = Some(node.into());
        self
    }

    pub fn end(&mut self, node: impl Into<String>) -> &mut Self {
        self.end = Some(node.into());
        self
    }

    pub fn node(&mut self, name: impl Into<String>, kind: NodeKind) -> &mut Self {
        self.nodes.insert(name.into(), kind);
        self
    }

    /// 添加边（无条件普通边）。
    ///
    /// 返回 [`PendingEdge`]，可通过 `.max_visits(n)` 附加循环分析约束。
    ///
    /// ```rust,ignore
    /// g.edge("a", "b");                    // 普通边
    /// g.edge("b", "a").max_visits(5);      // 普通边 + 循环分析
    /// ```
    pub fn edge(&mut self, from: impl Into<String>, to: impl Into<String>) -> PendingEdge<'_> {
        let edge_index = self.edges.len();
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            analysis: None,
            fallback: false,
        });
        PendingEdge {
            builder: self,
            edge_index,
        }
    }

    /// 添加条件边（`if/else-if` 规则链）。
    ///
    /// 返回 [`PendingEdge`]，可通过 `.max_visits(n)` 附加循环分析约束。
    ///
    /// ```rust,ignore
    /// g.edge_if("agent", "retry", |s| s.has_tool_calls()).max_visits(10);
    /// g.edge_if("agent", "end", |_| true);
    /// ```
    pub fn edge_if(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&State) -> bool + Send + Sync + 'static,
    ) -> PendingEdge<'_> {
        let edge_index = self.edges.len();
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: Some(Arc::new(condition)),
            analysis: None,
            fallback: false,
        });
        PendingEdge {
            builder: self,
            edge_index,
        }
    }

    /// 添加 fallback 边（无条件兜底）。
    ///
    /// 返回 [`PendingEdge`]，可通过 `.max_visits(n)` 附加循环分析约束。
    pub fn edge_fallback(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> PendingEdge<'_> {
        let edge_index = self.edges.len();
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            analysis: None,
            fallback: true,
        });
        PendingEdge {
            builder: self,
            edge_index,
        }
    }

    /// 构建 Graph。
    ///
    /// 收集所有错误后统一报告。`Warning` 变体不阻止 build 成功。
    ///
    /// ```rust,ignore
    /// match builder.build() {
    ///     Ok(graph) => { /* 使用 graph */ }
    ///     Err(errors) => {
    ///         for e in &errors.0 {
    ///             eprintln!("{}", e);
    ///         }
    ///     }
    /// }
    /// ```
    pub fn build(self) -> Result<Graph, BuildErrors> {
        let mut errors = BuildErrors::new();

        // 1. 检查入口/出口
        let start = match self.start {
            Some(s) => s,
            None => {
                errors.push(BuildError::MissingEntryPoint);
                // 无法继续验证，提前返回
                return Err(errors);
            }
        };
        let end = match self.end {
            Some(s) => s,
            None => {
                errors.push(BuildError::MissingExitPoint);
                return Err(errors);
            }
        };

        // 2. 检测重复节点名
        let mut seen_nodes = std::collections::HashSet::new();
        for name in self.nodes.keys() {
            if !seen_nodes.insert(name.clone()) {
                errors.push(BuildError::DuplicateNode { id: name.clone() });
            }
        }

        // 3. 检查边引用的节点是否存在
        for edge in &self.edges {
            if !self.nodes.contains_key(&edge.from) {
                errors.push(BuildError::MissingNode {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                });
            }
            if !self.nodes.contains_key(&edge.to) {
                errors.push(BuildError::MissingNode {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                });
            }
        }

        // 4. 有错误则返回（build() 是纯函数，不产生 Warning）
        if !errors.is_empty() {
            return Err(errors);
        }

        // 5. 构建 Graph
        let graph = Graph {
            name: self.name,
            nodes: self.nodes,
            edges: self.edges,
            start,
            end,
        };

        // 6. 结构验证（validate 检查 start/end 节点存在性等）
        if let Err(e) = graph.validate() {
            return Err(BuildErrors(vec![BuildError::InvalidEdgeDefinition {
                from: "graph".to_string(),
                to: "graph".to_string(),
                reason: e.to_string(),
            }]));
        }

        Ok(graph)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}
