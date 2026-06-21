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

use crate::error::{BuildError, BuildErrors, DiagnosticCategory, GraphDiagnostics};
use crate::node::{FlowNode, NodeKind};
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

    /// 计算图结构指纹 hash。
    ///
    /// 用于 Checkpoint 恢复时校验图结构是否变更。
    /// 基于节点名和边定义生成简化的 hash 字符串。
    pub fn hash(&self) -> String {
        let mut s = String::new();
        // 排序节点名，确保确定性
        let mut names: Vec<&str> = self.nodes.keys().map(|k| k.as_str()).collect();
        names.sort();
        s.push_str(&names.join(","));
        s.push('|');
        // 排序边，确保确定性
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
        // Simple hash — FNV-1a
        let hash = fnv_hash(&s);
        format!("{:016x}", hash)
    }

    pub fn edges_from(&self, from: &str) -> Vec<&Edge> {
        self.edges.iter().filter(|e| e.from == from).collect()
    }

    pub fn find_edge(&self, from: &str, to: &str) -> Option<&Edge> {
        self.edges.iter().find(|e| e.from == from && e.to == to)
    }

    /// 获取节点映射表引用。
    pub fn node_map(&self) -> &IndexMap<String, NodeKind> {
        &self.nodes
    }

    /// 路由解析 — 根据当前节点和 State 找到下一个节点。
    ///
    /// 三类边优先级：条件边 > 普通边 > Fallback 边。
    pub fn resolve_next(&self, current: &str, state: &State) -> Option<String> {
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

    /// 查找指定节点的 fallback 边目标。
    ///
    /// 用于 Fallback 控制流：节点主动声明降级后，Executor 查找 fallback 边路由。
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

    /// 完整图诊断分析。
    ///
    /// 检查以下维度并返回 `GraphDiagnostics`：
    /// 1. **环检测** — 图中存在循环路径（Warning）
    /// 2. **Fallback 参与循环** — fallback 边在环内（Warning）
    /// 3. **不可达路径** — 从 start 无法到达的节点（Info）
    /// 4. **End 节点出边** — end 节点定义了出边（Info）
    ///
    /// 与 `build()` 的关系：`build()` 只检查结构正确性；`analyze()` 检查风险性。
    pub fn analyze(&self) -> GraphDiagnostics {
        let mut diag = GraphDiagnostics::new();

        // 1. 构建邻接表（复用）
        let adj = self.build_adj();

        // 2. 环检测
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
            // 受保护的环仅提示
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

        // 3. Fallback 参与循环
        check_fallback_in_cycles(self, &cycles, &mut diag);

        // 4. 不可达路径（BFS 从 start 出发）
        check_unreachable_nodes(self, &adj, &mut diag);

        // 5. End 节点出边
        check_end_node_outgoing(self, &mut diag);

        diag
    }

    // ─── 内部辅助方法 ───────────────────────────────────────

    /// 构建邻接表。
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
    ///
    /// 仅用于嵌套场景（如 AgentFlowNode 内部 ReAct 循环）。
    /// 只包含"路由解析 + 节点执行"的纯逻辑，剥离了：
    /// - RuntimeEvent 发射
    /// - Checkpoint
    /// - Barrier 等待（内联模式不支持 Pause 信号）
    ///
    /// # 参数
    /// - `ctx` — 节点上下文
    /// - `max_steps` — 最大步数（防止无限循环）
    ///
    /// # 返回
    /// - `Ok(())` — 正常结束（到达 end 节点）
    /// - `Err(GraphError)` — 执行出错
    pub async fn run_inline(
        &self,
        ctx: &mut crate::node_context::NodeContext<'_>,
        max_steps: usize,
    ) -> Result<(), crate::error::GraphError> {
        use crate::node_context::NextAction;
        use crate::stream_emitter::StreamEmitter;

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

            // 创建临时 StreamEmitter（内联模式）
            let (tx, _rx) = tokio::sync::mpsc::channel(64);
            let _emitter = StreamEmitter::new(tx);

            // 创建子 BranchState
            let mut branch = ctx.state().fork();
            // 透传父 ctx 的 StreamEmitter（v04 #1: 让内联节点也能发射流式事件）
            let stream = ctx.stream();
            let mut child_ctx = crate::node_context::NodeContext::new(&mut branch, stream);

            // 执行节点
            node.execute(&mut child_ctx).await?;

            // 提取控制信号
            let (next_action, _signal) = child_ctx.take_control();

            // 将变更 apply 回父 ctx
            for change in branch.changes() {
                match change.operation {
                    crate::branch_state::ChangeOperation::Put => {
                        ctx.set(&change.key, change.value.clone());
                    }
                    crate::branch_state::ChangeOperation::Delete => {
                        ctx.remove(&change.key);
                    }
                }
            }

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
                    // 解析下一个节点
                    current = self.resolve_next_inline(&current, ctx.state())?;
                }
            }
        }
    }

    /// 内联路由解析（简化版，不支持 Barrier）。
    fn resolve_next_inline(
        &self,
        current: &str,
        state: &crate::branch_state::BranchState,
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
            if edge.is_conditional() {
                let full_state = state.to_state();
                if edge.condition.as_ref().is_some_and(|c| c(&full_state)) {
                    return Ok(edge.to.clone());
                }
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

    /// DFS 环检测。
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

    /// 过滤未受保护的环。
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

    // ─── 兼容方法 ─────────────────────────────────────────────

    /// 分析图中所有环，生成诊断信息。
    ///
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

// ─── 诊断辅助函数 ───────────────────────────────────────────────

/// 格式化环路径为字符串："a → b → c"
fn format_cycle(cycle: &[String]) -> String {
    cycle.join(" → ")
}

/// 检查 fallback 边是否参与循环。
fn check_fallback_in_cycles(graph: &Graph, cycles: &[Vec<String>], diag: &mut GraphDiagnostics) {
    // 收集所有 fallback 边的 (from, to)
    let fallback_edges: std::collections::HashSet<(&str, &str)> = graph
        .edges
        .iter()
        .filter(|e| e.fallback)
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();

    if fallback_edges.is_empty() {
        return;
    }

    // 检查每个环是否包含 fallback 边
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

/// 检查从 start 节点不可达的节点。
fn check_unreachable_nodes(
    graph: &Graph,
    adj: &std::collections::HashMap<String, Vec<String>>,
    diag: &mut GraphDiagnostics,
) {
    // BFS 从 start 出发
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

    // 找出未访问的节点
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

/// 检查 end 节点是否有出边。
fn check_end_node_outgoing(graph: &Graph, diag: &mut GraphDiagnostics) {
    let outgoing: Vec<&Edge> = graph.edges.iter().filter(|e| e.from == graph.end).collect();

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

/// FNV-1a hash — 无外部依赖的简单 hash。
fn fnv_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
