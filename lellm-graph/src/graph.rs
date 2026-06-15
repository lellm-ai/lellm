//! Graph 和 GraphBuilder。
//!
//! Edge 三层语义：
//! - `condition` — 业务路由条件（必须满足）
//! - `analysis` — 分析用约束（不参与 runtime 决策）
//! - `policy` — runtime policy（显式声明才生效）
//!
//! `fallback` — 兜底边，无匹配时优先尝试。

use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::BuildError;
use crate::node::NodeKind;
use crate::state::State;

// ─── Edge ──────────────────────────────────────────────────────

/// 边条件回调类型别名。
/// Arc 包装以支持 Graph Clone（条件回调不可 Clone）。
pub type EdgeCondition = Arc<dyn Fn(&State) -> bool + Send + Sync>;

/// 边（Edge）— 三层语义叠加。
pub struct Edge {
    pub from: String,
    pub to: String,
    /// ① 业务路由条件（必须满足）
    pub condition: Option<EdgeCondition>,
    /// ② 分析用约束（不参与 runtime 决策）
    pub analysis: Option<EdgeAnalysis>,
    /// ③ runtime policy（显式声明才生效）
    pub policy: Option<EdgePolicy>,
    /// ④ fallback 标记 — 兜底边
    pub fallback: bool,
}

/// 分析用约束 — 仅用于 `analyze_cycles()` 静态分析。
///
/// `analysis` = "你可能会出事"，不参与执行控制。
#[derive(Debug, Clone)]
pub struct EdgeAnalysis {
    /// 建议的最大访问次数 — 用于循环分析诊断。
    /// 不参与运行时拦截。
    pub max_visits: Option<usize>,
}

/// Runtime Policy — 显式声明的运行时拦截策略。
///
/// `policy` = "我现在要拦你"，参与执行控制。
#[derive(Debug, Clone)]
pub enum EdgePolicy {
    /// 限制边被 traversed 的次数。超过后按策略处理。
    MaxVisits { limit: usize, on_exceeded: EdgeExceededStrategy },
}

/// Edge Policy 被 exceeded 时的处理策略。
#[derive(Debug, Clone, Copy, Default)]
pub enum EdgeExceededStrategy {
    /// 严格模式（默认）— 路径失败，回溯到上一个 decision node
    #[default]
    Strict,
    /// 软降级 — 尝试其他满足 condition 的 edge → fallback → 失败
    SoftFallback,
    /// 静默跳过 — 不报错，继续执行其他逻辑
    Drop,
}

// ─── Graph ─────────────────────────────────────────────────────

/// 图（Graph）— 允许有环，循环保护由 GraphExecutor::max_steps 运行时熔断提供。
pub struct Graph {
    pub(crate) nodes: IndexMap<String, NodeKind>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) start: String,
    pub(crate) end: String,
}

impl Graph {
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
    /// 用于 RecoverableError 恢复：当边级 policy 触发 SoftFallback 时，
    /// 寻找 fallback 边作为降级路径。
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
            adj.entry(edge.from.clone()).or_default().push(edge.to.clone());
        }

        for node in self.nodes.keys() {
            let mut in_path = std::collections::HashSet::new();
            path.clear();
            self.dfs_cycles(node, node, &adj, &mut in_path, &mut path, &mut cycles);
        }

        // 检查哪些环有 analysis 或 policy 保护
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
                            && (e.analysis.as_ref().is_some_and(|a| a.max_visits.is_some())
                                || e.policy.is_some())
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
            protected_edges: self.edges.iter().filter(|e| {
                e.analysis.as_ref().is_some_and(|a| a.max_visits.is_some()) || e.policy.is_some()
            }).count(),
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
            "Edge protection: {}/{} edges have analysis or policy set.",
            self.protected_edges, self.total_edges
        ));

        for (i, cycle) in self.cycles.iter().enumerate() {
            let cycle_str = cycle.join(" → ");
            lines.push(format!("  Cycle {}: {} → {}", i + 1, cycle_str, cycle[0]));

            if self.unprotected_cycles.contains(cycle) {
                lines.push("    ⚠️ UNPROTECTED — no max_visits or policy on back-edge".into());
            } else {
                lines.push("    ✅ Protected by edge-level analysis or policy".into());
            }
        }

        if !self.all_protected() {
            lines.push("".into());
            lines.push(
                "⚠️ Recommendation: Set analysis.max_visits or policy on back-edges."
                    .to_string(),
            );
        }

        lines.join("\n")
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

    pub fn start(&mut self, node: impl Into<String>) -> Result<&mut Self, BuildError> {
        self.start = Some(node.into());
        Ok(self)
    }

    pub fn end(&mut self, node: impl Into<String>) -> Result<&mut Self, BuildError> {
        self.end = Some(node.into());
        Ok(self)
    }

    pub fn node(
        &mut self,
        name: impl Into<String>,
        kind: NodeKind,
    ) -> Result<&mut Self, BuildError> {
        let name = name.into();
        if self.nodes.contains_key(&name) {
            return Err(BuildError::DuplicateNode { id: name });
        }
        self.nodes.insert(name, kind);
        Ok(self)
    }

    /// 添加边（无条件，无 policy）。
    pub fn edge(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Result<&mut Self, BuildError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            analysis: None,
            policy: None,
            fallback: false,
        });
        Ok(self)
    }

    /// 添加条件边。
    pub fn edge_if(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&State) -> bool + Send + Sync + 'static,
    ) -> Result<&mut Self, BuildError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: Some(Arc::new(condition)),
            analysis: None,
            policy: None,
            fallback: false,
        });
        Ok(self)
    }

    /// 添加 fallback 边（无条件兜底）。
    pub fn edge_fallback(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Result<&mut Self, BuildError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            analysis: None,
            policy: None,
            fallback: true,
        });
        Ok(self)
    }

    /// 添加带 analysis 约束的边（仅静态分析用，不参与 runtime）。
    pub fn edge_analysis(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        max_visits: usize,
    ) -> Result<&mut Self, BuildError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            analysis: Some(EdgeAnalysis {
                max_visits: Some(max_visits),
            }),
            policy: None,
            fallback: false,
        });
        Ok(self)
    }

    /// 添加带 runtime policy 的边（显式拦截）。
    pub fn edge_policy(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        policy: EdgePolicy,
    ) -> Result<&mut Self, BuildError> {
        self.edges.push(Edge {
            from: from.into(),
            to: to.into(),
            condition: None,
            analysis: None,
            policy: Some(policy),
            fallback: false,
        });
        Ok(self)
    }

    /// 构建 Graph。返回 `Result<Graph, BuildError>`。
    pub fn build(self) -> Result<Graph, BuildError> {
        let start = self.start.ok_or(BuildError::MissingEntryPoint)?;
        let end = self.end.ok_or(BuildError::MissingExitPoint)?;

        let graph = Graph {
            nodes: self.nodes,
            edges: self.edges,
            start,
            end,
        };

        // 结构验证
        for edge in &graph.edges {
            if !graph.nodes.contains_key(&edge.from) {
                return Err(BuildError::MissingNode {
                    from: edge.from.clone(),
                    to: edge.from.clone(),
                });
            }
            if !graph.nodes.contains_key(&edge.to) {
                return Err(BuildError::MissingNode {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                });
            }
        }

        graph.validate().map_err(|e| match e {
            crate::error::TerminalError::InvalidGraph(msg) => {
                BuildError::InvalidEdgeDefinition {
                    from: "unknown".into(),
                    to: "unknown".into(),
                    reason: msg,
                }
            }
            _ => BuildError::InvalidEdgeDefinition {
                from: "unknown".into(),
                to: "unknown".into(),
                reason: "validation failed".into(),
            },
        })?;

        Ok(graph)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}
