//! Graph 和 GraphBuilder。
//!
//! Edge 三类边模型：
//! - **条件边** (`edge_if`) — `if/else-if` 规则链，按注册顺序求值，first match wins
//! - **普通边** (`edge`) — 无条件非 fallback，条件边无命中时生效
//! - **Fallback 边** (`edge_fallback`) — 最后兜底
//!
//! v0.4+: 泛型化 `Graph<S: WorkflowState>`，默认 `S = State`（向后兼容）。
//!
//! 运行时安全由 `GraphExecutor::max_steps` 统一负责。

use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::{BuildError, BuildErrors, DiagnosticCategory, GraphDiagnostics};
use crate::node::{FlowNode, NodeKind};
use crate::node_context::NodeContext;
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};

// ─── Edge ──────────────────────────────────────────────────────

/// 边条件回调类型别名。
pub type EdgeCondition<S> = Arc<dyn Fn(&S) -> bool + Send + Sync>;

/// 边（Edge）— 三类边模型。
#[derive(Clone)]
pub struct Edge<S: WorkflowState = State> {
    pub from: String,
    pub to: String,
    /// 路由条件。Some = 条件边；None = 普通边或 fallback 边。
    pub condition: Option<EdgeCondition<S>>,
    /// 分析用约束（不参与 runtime 决策）
    pub analysis: Option<EdgeAnalysis>,
    /// 是否为 fallback 边（最后兜底）
    pub fallback: bool,
}

impl<S: WorkflowState> Edge<S> {
    /// 判断是否为条件边。
    pub fn is_conditional(&self) -> bool {
        self.condition.is_some() && !self.fallback
    }

    /// 判断是否为普通边（无条件非 fallback）。
    pub fn is_normal(&self) -> bool {
        self.condition.is_none() && !self.fallback
    }
}

impl<S: WorkflowState> std::fmt::Debug for Edge<S> {
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
#[derive(Debug, Clone)]
pub struct EdgeAnalysis {
    /// 建议的最大访问次数
    pub max_visits: Option<usize>,
}

// ─── Graph ─────────────────────────────────────────────────────

/// 图（Graph）— 允许有环，循环保护由 GraphExecutor::max_steps 运行时熔断提供。
#[derive(Clone)]
pub struct Graph<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    pub(crate) name: String,
    pub(crate) nodes: IndexMap<String, NodeKind<S, M>>,
    pub(crate) edges: Vec<Edge<S>>,
    pub(crate) start: String,
    pub(crate) end: String,
}

impl<S: WorkflowState, M: MergeStrategy<S>> Graph<S, M> {
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

    /// 计算图结构指纹 hash。
    pub fn hash(&self) -> String {
        let mut s = String::new();
        let mut names: Vec<&str> = self.nodes.keys().map(|k| k.as_str()).collect();
        names.sort();
        s.push_str(&names.join(","));
        s.push('|');
        let mut edge_strs: Vec<String> = self
            .edges
            .iter()
            .map(|e| {
                format!(
                    "{}->{}{:?}{}",
                    e.from,
                    e.to,
                    if e.condition.is_some() { "?" } else { "" },
                    if e.fallback { "!" } else { "" }
                )
            })
            .collect();
        edge_strs.sort();
        s.push_str(&edge_strs.join(","));
        let hash = fnv_hash(&s);
        format!("{:016x}", hash)
    }

    pub fn edges_from(&self, from: &str) -> Vec<&Edge<S>> {
        self.edges.iter().filter(|e| e.from == from).collect()
    }

    pub fn find_edge(&self, from: &str, to: &str) -> Option<&Edge<S>> {
        self.edges.iter().find(|e| e.from == from && e.to == to)
    }

    /// 获取节点映射表引用。
    pub fn node_map(&self) -> &IndexMap<String, NodeKind<S, M>> {
        &self.nodes
    }

    /// 路由解析 — 根据当前节点和 State 找到下一个节点。
    pub fn resolve_next(&self, current: &str, state: &S) -> Option<String> {
        let edges = self.edges_from(current);

        // 1. 条件边
        for edge in &edges {
            if edge.is_conditional() && edge.condition.as_ref().is_some_and(|c| c(state)) {
                return Some(edge.to.clone());
            }
        }

        // 2. 普通边
        for edge in &edges {
            if edge.is_normal() {
                return Some(edge.to.clone());
            }
        }

        // 3. Fallback 边
        for edge in &edges {
            if edge.fallback {
                return Some(edge.to.clone());
            }
        }

        None
    }

    pub fn find_fallback_edge(&self, from: &str) -> Option<String> {
        self.edges
            .iter()
            .find(|e| e.from == from && e.fallback)
            .map(|e| e.to.clone())
    }

    /// 验证图结构。
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

    /// 完整图诊断分析。
    pub fn analyze(&self) -> GraphDiagnostics {
        let mut diag = GraphDiagnostics::new();
        let adj = self.build_adj();

        let cycles = self.find_all_cycles(&adj);
        if !cycles.is_empty() {
            let unprotected = self.filter_unprotected_cycles(&cycles);
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

        check_fallback_in_cycles(self, &cycles, &mut diag);
        check_unreachable_nodes(self, &adj, &mut diag);
        check_end_node_outgoing(self, &mut diag);

        diag
    }

    fn build_adj(&self) -> std::collections::HashMap<String, Vec<String>> {
        let mut adj: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for edge in &self.edges {
            adj.entry(edge.from.clone())
                .or_default()
                .push(edge.to.clone());
        }
        adj
    }

    // ─── 内联执行 ────────────────────────────────────────────

    /// 内联执行 — 不产生 RuntimeEvent，不 Checkpoint。
    pub async fn run_inline(
        &self,
        ctx: &mut NodeContext<'_, S>,
        max_steps: usize,
    ) -> Result<(), crate::error::GraphError> {
        use crate::node_context::NextAction;

        let mut current = self.start_node().to_string();
        let mut step: usize = 0;

        loop {
            step += 1;
            if step > max_steps {
                return Err(crate::error::GraphError::Terminal(
                    crate::error::TerminalError::StepsExceeded { limit: max_steps },
                ));
            }

            let node = self.nodes.get(&current).ok_or_else(|| {
                crate::error::GraphError::Terminal(crate::error::TerminalError::NodeNotFound(
                    current.clone(),
                ))
            })?;

            // 执行节点
            node.execute(ctx).await?;

            // 消费 Effects → apply 到 typed state（零序列化）
            let effects = ctx.consume_effects();
            ctx.state_mut().apply_batch(effects);

            // 提取控制信号
            let (next_action, _signal) = ctx.take_control();

            // 处理路由
            match next_action {
                NextAction::End => return Ok(()),
                NextAction::Goto(target) => {
                    current = target;
                }
                NextAction::Next => {
                    if current == self.end_node() {
                        return Ok(());
                    }
                    current = self.resolve_next_inline(&current, ctx.state())?;
                }
            }
        }
    }

    /// 内联路由解析。
    fn resolve_next_inline(
        &self,
        current: &str,
        state: &S,
    ) -> Result<String, crate::error::GraphError> {
        let edges = self.edges_from(current);

        if edges.is_empty() {
            return Err(crate::error::GraphError::Terminal(
                crate::error::TerminalError::InvalidGraph(format!(
                    "node '{}' has no outgoing edges and is not the end node",
                    current
                )),
            ));
        }

        // 1. 条件边
        for edge in &edges {
            if edge.is_conditional() && edge.condition.as_ref().is_some_and(|c| c(state)) {
                return Ok(edge.to.clone());
            }
        }

        // 2. 普通边
        for edge in &edges {
            if edge.is_normal() {
                return Ok(edge.to.clone());
            }
        }

        // 3. Fallback 边
        for edge in &edges {
            if edge.fallback {
                return Ok(edge.to.clone());
            }
        }

        Err(crate::error::GraphError::Terminal(
            crate::error::TerminalError::InvalidGraph(format!(
                "node '{}' has no matching outgoing edge",
                current
            )),
        ))
    }

    /// 查找所有环。
    fn find_all_cycles(
        &self,
        adj: &std::collections::HashMap<String, Vec<String>>,
    ) -> Vec<Vec<String>> {
        let mut cycles = Vec::new();
        for node in self.nodes.keys() {
            let mut in_path = std::collections::HashSet::new();
            let mut path = Vec::new();
            self.dfs_cycles(node, node, adj, &mut in_path, &mut path, &mut cycles);
        }
        cycles
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

    fn filter_unprotected_cycles(&self, cycles: &[Vec<String>]) -> Vec<Vec<String>> {
        let mut unprotected: Vec<Vec<String>> = cycles
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
            .collect();
        unprotected.sort();
        unprotected.dedup();
        unprotected
    }

    /// @deprecated 使用 [`analyze()`](Self::analyze) 替代。
    pub fn analyze_cycles(&self) -> CycleAnalysis {
        let adj = self.build_adj();
        let cycles = self.find_all_cycles(&adj);
        let unprotected = self.filter_unprotected_cycles(&cycles);

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
pub struct PendingEdge<'a, S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    builder: &'a mut GraphBuilder<S, M>,
    edge_index: usize,
}

impl<'a, S: WorkflowState, M: MergeStrategy<S>> PendingEdge<'a, S, M> {
    pub fn max_visits(self, n: usize) -> &'a mut GraphBuilder<S, M> {
        self.builder.edges[self.edge_index].analysis = Some(EdgeAnalysis {
            max_visits: Some(n),
        });
        self.builder
    }
}

// ─── GraphBuilder ─────────────────────────────────────────────

/// Graph 构建器。
pub struct GraphBuilder<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    name: String,
    nodes: IndexMap<String, NodeKind<S, M>>,
    edges: Vec<Edge<S>>,
    start: Option<String>,
    end: Option<String>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> GraphBuilder<S, M> {
    /// 创建 GraphBuilder。
    ///
    /// 类型参数由调用方推断或显式指定。
    /// - 默认: `GraphBuilder::new("name")` → `GraphBuilder<State, StateMerge>`
    /// - 自定义: `GraphBuilder::<AgentState, _>::new("name")`
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

    pub fn node(&mut self, name: impl Into<String>, kind: NodeKind<S, M>) -> &mut Self {
        self.nodes.insert(name.into(), kind);
        self
    }

    pub fn edge(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> PendingEdge<'_, S, M> {
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

    pub fn edge_if(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        condition: impl Fn(&S) -> bool + Send + Sync + 'static,
    ) -> PendingEdge<'_, S, M> {
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

    pub fn edge_fallback(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> PendingEdge<'_, S, M> {
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

    pub fn build(self) -> Result<Graph<S, M>, BuildErrors> {
        let mut errors = BuildErrors::new();

        let start = match self.start {
            Some(s) => s,
            None => {
                errors.push(BuildError::MissingEntryPoint);
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

        let mut seen_nodes = std::collections::HashSet::new();
        for name in self.nodes.keys() {
            if !seen_nodes.insert(name.clone()) {
                errors.push(BuildError::DuplicateNode { id: name.clone() });
            }
        }

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

        if !errors.is_empty() {
            return Err(errors);
        }

        let graph = Graph {
            name: self.name,
            nodes: self.nodes,
            edges: self.edges,
            start,
            end,
        };

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

fn fnv_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
