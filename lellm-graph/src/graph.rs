//! Graph 和 GraphBuilder。
//!
//! Edge 三类边模型：
//! - **条件边** (`edge_if`) — `if/else-if` 规则链，按注册顺序求值，first match wins
//! - **普通边** (`edge`) — 无条件非 fallback，条件边无命中时生效
//! - **Fallback 边** (`edge_fallback`) — 最后兜底
//!
//! v0.4+: 泛型化 `Graph<S: WorkflowState>`，默认 `S = State`（向后兼容）。
//!
//! 运行时安全由 `run_inline()` 的 `max_steps` 参数负责。

use std::sync::Arc;

use indexmap::IndexMap;

use crate::error::{BuildError, BuildErrors, GraphDiagnostics, GraphError, TerminalError};
use crate::execution_engine::{ExecutionEngine, ExecutorState, NextAction};
use crate::graph_analysis::{self, CycleAnalysis};
use crate::node::{BarrierNode, ConditionNode, FlowNode, LeafNode, NodeKind};
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

    /// 计算图结构指纹 hash（u64 原始值）。
    ///
    /// 用于 Checkpoint 的 graph compatibility 校验。
    pub fn hash_u64(&self) -> u64 {
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
        fnv_hash(&s)
    }

    /// 计算图结构指纹 hash（hex 字符串）。
    pub fn hash(&self) -> String {
        format!("{:016x}", self.hash_u64())
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

    /// 路由解析 — 根据当前节点和 State 找到下一个节点（返回 Option）。
    ///
    /// 内部统一使用的边评估逻辑。无匹配时返回 `None`（不区分"无边"和"无匹配"）。
    fn resolve_next(&self, current: &str, state: &S) -> Option<String> {
        // 1. 条件边
        for edge in self.edges_from(current) {
            if edge.is_conditional() && edge.condition.as_ref().is_some_and(|c| c(state)) {
                return Some(edge.to.clone());
            }
        }

        // 2. 普通边
        for edge in self.edges_from(current) {
            if edge.is_normal() {
                return Some(edge.to.clone());
            }
        }

        // 3. Fallback 边
        for edge in self.edges_from(current) {
            if edge.fallback {
                return Some(edge.to.clone());
            }
        }

        None
    }

    /// 路由解析 — 内联执行使用，无匹配时返回错误。
    pub(crate) fn resolve_next_inline(
        &self,
        current: &str,
        state: &S,
    ) -> Result<String, GraphError> {
        if self.edges_from(current).is_empty() {
            return Err(GraphError::Terminal(TerminalError::InvalidGraph(format!(
                "node '{}' has no outgoing edges and is not the end node",
                current
            ))));
        }

        self.resolve_next(current, state).ok_or_else(|| {
            GraphError::Terminal(TerminalError::InvalidGraph(format!(
                "node '{}' has no matching outgoing edge",
                current
            )))
        })
    }

    pub fn find_fallback_edge(&self, from: &str) -> Option<String> {
        self.edges
            .iter()
            .find(|e| e.from == from && e.fallback)
            .map(|e| e.to.clone())
    }

    /// 验证图结构。
    pub fn validate(&self) -> Result<(), TerminalError> {
        if !self.nodes.contains_key(&self.start) {
            return Err(TerminalError::InvalidGraph(format!(
                "start node '{}' not found",
                self.start
            )));
        }

        if !self.nodes.contains_key(&self.end) {
            return Err(TerminalError::InvalidGraph(format!(
                "end node '{}' not found",
                self.end
            )));
        }

        for edge in &self.edges {
            if !self.nodes.contains_key(&edge.from) {
                return Err(TerminalError::InvalidGraph(format!(
                    "edge references non-existent source node '{}'",
                    edge.from
                )));
            }
            if !self.nodes.contains_key(&edge.to) {
                return Err(TerminalError::InvalidGraph(format!(
                    "edge references non-existent target node '{}'",
                    edge.to
                )));
            }
        }

        Ok(())
    }

    /// 完整图诊断分析。
    pub fn analyze(&self) -> GraphDiagnostics {
        graph_analysis::analyze_graph(self)
    }

    /// @deprecated 使用 [`analyze()`](Self::analyze) 替代。
    pub fn analyze_cycles(&self) -> CycleAnalysis {
        let cycles = graph_analysis::find_all_cycles(self);
        let unprotected = graph_analysis::filter_unprotected_cycles(self, &cycles);

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

    // ─── 内联执行 ────────────────────────────────────────────

    /// 内联执行 — 不产生 RuntimeEvent，不 Checkpoint。
    ///
    /// 接收 [`ExecutionEngine`]（拥有者），内部循环构建 [`NodeContext`]（能力视图）
    /// 供节点使用。执行完毕后通过 `take_*` 消费 Mutation 和控制信号。
    ///
    /// 数据流：
    /// ```text
    /// ExecutionEngine
    ///   → build_node_context()  → NodeContext<'_, S>
    ///   → node.execute(ctx)     → 节点 record() Mutations
    ///   → drop(ctx)             → 释放借用
    ///   → take_mutations()      → 消费 Mutation 缓冲
    ///   → state.apply_batch()   → apply 到 State
    ///   → take_control()        → 获取路由信号
    /// ```
    pub async fn run_inline(
        &self,
        exec_ctx: &mut ExecutionEngine<'_, S>,
        max_steps: usize,
    ) -> Result<(), GraphError> {
        let mut current = self.start_node().to_string();
        let mut step: usize = 0;

        loop {
            step += 1;
            if step > max_steps {
                return Err(GraphError::Terminal(TerminalError::StepsExceeded {
                    limit: max_steps,
                }));
            }

            let node = self.nodes.get(&current).ok_or_else(|| {
                GraphError::Terminal(TerminalError::NodeNotFound(current.clone()))
            })?;

            // 根据 NodeKind 分发执行
            match node {
                NodeKind::Task(n) => {
                    let mut ctx = exec_ctx.build_node_context();
                    n.execute(&mut ctx).await?;
                }
                NodeKind::Condition(n) => {
                    let mut ctx = exec_ctx.build_leaf_context();
                    <ConditionNode<S> as LeafNode<S>>::execute(n, &mut ctx).await?;
                }
                NodeKind::Barrier(n) => {
                    let mut ctx = exec_ctx.build_leaf_context();
                    <BarrierNode<S> as LeafNode<S>>::execute(n, &mut ctx).await?;
                }
                NodeKind::External(n) => {
                    let mut ctx = exec_ctx.build_node_context();
                    n.execute(&mut ctx).await?;
                }
                NodeKind::ExternalLeaf(n) => {
                    let mut ctx = exec_ctx.build_leaf_context();
                    n.execute(&mut ctx).await?;
                }
                NodeKind::Parallel(p) => {
                    // ExecutorOperation 直接接收 &mut ExecutionEngine
                    p.execute(exec_ctx).await?;
                }
                NodeKind::Subgraph(_subgraph) => {
                    // TODO: 实现 Subgraph 执行
                    // 由 ExecutionEngine 负责 Frame 管理、状态投影、Checkpoint 和恢复
                    tracing::warn!("Subgraph execution not yet implemented");
                }
            }

            // commit mutations (Unit of Work) — 对 Parallel 是空操作
            exec_ctx.commit();

            // 消费 FlowEvent 缓冲（积累到 engine，执行结束后由调用者取用）
            let _flow_events = exec_ctx.take_flow_events();

            // 提取控制信号
            let (next_action, _signal) = exec_ctx.take_control();

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
                    current = self.resolve_next_inline(&current, exec_ctx.state())?;
                }
            }
        }
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

// ─── Utilities ─────────────────────────────────────────────────

fn fnv_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
