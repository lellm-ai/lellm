//! Graph 静态分析 — 环检测、不可达节点、Fallback 诊断。
//!
//! 从 graph.rs 拆分出来，保持核心文件精简。

use crate::error::{DiagnosticCategory, GraphDiagnostics};
use crate::graph::{Edge, Graph};
use crate::workflow_state::{MergeStrategy, WorkflowState};

// ─── 环检测 ──────────────────────────────────────────────────────

/// 查找所有环。
pub(crate) fn find_all_cycles<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
) -> Vec<Vec<String>> {
    let adj = build_adj(graph);
    let mut cycles = Vec::new();
    for node in graph.nodes.keys() {
        let mut in_path = std::collections::HashSet::new();
        let mut path = Vec::new();
        dfs_cycles(node, node, &adj, &mut in_path, &mut path, &mut cycles);
    }
    cycles
}

fn dfs_cycles(
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
                dfs_cycles(start, neighbor, adj, in_path, path, cycles);
            }
        }
    }

    path.pop();
    in_path.remove(current);
}

/// 过滤未保护的环（环上所有边都没有 max_visits 约束）。
pub(crate) fn filter_unprotected_cycles<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
    cycles: &[Vec<String>],
) -> Vec<Vec<String>> {
    let mut unprotected: Vec<Vec<String>> = cycles
        .iter()
        .filter(|cycle| {
            let has_protection = (0..cycle.len()).any(|i| {
                let next = (i + 1) % cycle.len();
                let from = cycle[i].as_str();
                let to = cycle[next].as_str();
                graph.edges.iter().any(|e| {
                    e.from == from
                        && e.to == to
                        && e.analysis.as_ref().is_some_and(|a| a.max_visits.is_some())
                })
            });
            !has_protection
        })
        .cloned()
        .collect();
    unprotected.sort();
    unprotected.dedup();
    unprotected
}

/// 构建邻接表。
pub(crate) fn build_adj<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut adj: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for edge in &graph.edges {
        adj.entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }
    adj
}

// ─── CycleAnalysis ─────────────────────────────────────────────

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

// ─── Graph::analyze() 调用的分析逻辑 ─────────────────────────

/// 执行完整图诊断分析（由 Graph::analyze() 调用）。
pub(crate) fn analyze_graph<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
) -> GraphDiagnostics {
    let mut diag = GraphDiagnostics::new();
    let adj = build_adj(graph);

    let cycles = find_all_cycles(graph);
    if !cycles.is_empty() {
        let unprotected = filter_unprotected_cycles(graph, &cycles);
        for cycle in &unprotected {
            let cycle_str = format_cycle(cycle);
            diag.add_warning(
                DiagnosticCategory::Cycle,
                format!("cycle detected: {} → {}", cycle_str, cycle[0]),
            );
        }
        for cycle in &cycles {
            if !unprotected.contains(cycle) {
                let cycle_str = format_cycle(cycle);
                diag.add_info(
                    DiagnosticCategory::Cycle,
                    format!(
                        "protected cycle: {} → {} (has max_visits)",
                        cycle_str, cycle[0]
                    ),
                );
            }
        }
    }

    check_fallback_in_cycles(graph, &cycles, &mut diag);
    check_unreachable_nodes(graph, &adj, &mut diag);
    check_end_node_outgoing(graph, &mut diag);

    diag
}

// ─── 诊断辅助函数 ───────────────────────────────────────────────

fn format_cycle(cycle: &[String]) -> String {
    cycle.join(" → ")
}

fn check_fallback_in_cycles<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
    cycles: &[Vec<String>],
    diag: &mut GraphDiagnostics,
) {
    let fallback_edges: std::collections::HashSet<(&str, &str)> = graph
        .edges
        .iter()
        .filter(|e| e.fallback)
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();

    if fallback_edges.is_empty() {
        return;
    }

    for cycle in cycles {
        for i in 0..cycle.len() {
            let next = (i + 1) % cycle.len();
            let from = cycle[i].as_str();
            let to = cycle[next].as_str();
            if fallback_edges.contains(&(from, to)) {
                let edge_str = format!("{} → {}", from, to);
                diag.add_warning(
                    DiagnosticCategory::FallbackInCycle,
                    format!(
                        "fallback edge {} participates in cycle: {} → {}",
                        edge_str,
                        format_cycle(cycle),
                        cycle[0]
                    ),
                );
            }
        }
    }
}

fn check_unreachable_nodes<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
    adj: &std::collections::HashMap<String, Vec<String>>,
    diag: &mut GraphDiagnostics,
) {
    let mut visited = std::collections::HashSet::new();
    let mut queue = Vec::new();

    queue.push(graph.start.clone());
    visited.insert(graph.start.clone());

    while let Some(node) = queue.pop() {
        if let Some(neighbors) = adj.get(&node) {
            for neighbor in neighbors {
                if visited.insert(neighbor.clone()) {
                    queue.push(neighbor.clone());
                }
            }
        }
    }

    for name in graph.nodes.keys() {
        if !visited.contains(name) {
            diag.add_info(
                DiagnosticCategory::Unreachable,
                format!(
                    "node '{}' is not reachable from start node '{}'",
                    name, graph.start
                ),
            );
        }
    }
}

fn check_end_node_outgoing<S: WorkflowState, M: MergeStrategy<S>>(
    graph: &Graph<S, M>,
    diag: &mut GraphDiagnostics,
) {
    let outgoing: Vec<&Edge<S>> = graph.edges.iter().filter(|e| e.from == graph.end).collect();

    if !outgoing.is_empty() {
        let targets: Vec<&str> = outgoing.iter().map(|e| e.to.as_str()).collect();
        diag.add_info(
            DiagnosticCategory::EndNodeOutgoing,
            format!(
                "end node '{}' has {} outgoing edge(s) to: {:?}",
                graph.end,
                outgoing.len(),
                targets
            ),
        );
    }
}
