//! GraphBuilder — Graph AST 构建器。
//!
//! 提供链式 API 构建 [`Graph`](crate::Graph)，支持 `build()`（仅验证）
//! 和 `compile()`（验证 + 优化 pass）。

use std::sync::Arc;

use indexmap::IndexMap;

use super::{Edge, EdgeAnalysis, Graph};
use crate::error::{BuildError, BuildErrors};
use crate::node::NodeKind;
use crate::state::workflow_state::{MergeStrategy, WorkflowState};
use crate::state::{State, StateMerge};

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
    /// P0-2: 可选的 canonical hash — 如果 DSL 层设置了就使用，否则计算结构 hash。
    canonical_hash: Option<u64>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> GraphBuilder<S, M> {
    /// 创建 GraphBuilder。
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            nodes: IndexMap::new(),
            edges: Vec::new(),
            start: None,
            end: None,
            canonical_hash: None,
        }
    }

    /// P0-2: 设置 canonical hash — 由 DSL 层（如 AgentBuilder）调用。
    pub fn canonical_hash(&mut self, hash: u64) -> &mut Self {
        self.canonical_hash = Some(hash);
        self
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

    /// 便捷方法 — 添加 Subgraph 节点。
    pub fn subgraph<Inner: WorkflowState, IM: MergeStrategy<Inner>, L: crate::StateLens<S, Inner>>(
        &mut self,
        name: impl Into<String>,
        spec: crate::SubgraphSpec<S, Inner, IM, L>,
    ) -> &mut Self
    where
        S: 'static,
        Inner: 'static,
        IM: 'static,
        L: 'static,
    {
        let compiled = spec.compile();
        self.nodes.insert(name.into(), NodeKind::Subgraph(compiled));
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

        let structural_hash = compute_structural_hash(&self.nodes, &self.edges);

        let graph = Graph {
            name: self.name,
            nodes: self.nodes,
            edges: self.edges,
            start,
            end,
            canonical_hash: self.canonical_hash.unwrap_or(structural_hash),
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

    /// 构建并编译 — 在 `build()` 之后运行 Compiler Pass（如 InlinePass）。
    pub fn compile(self) -> Result<Graph<S, M>, BuildErrors> {
        use crate::compiler::CompilerPass;

        let mut graph = self.build()?;

        let mut ctx = crate::compiler::CompilerContext::<S>::new();
        let pass = crate::compiler::InlinePass::new();
        pass.run(&mut graph, &mut ctx);

        if ctx.debug {
            tracing::debug!(
                inlined = ctx.stats.inlined_count,
                skipped = ctx.stats.not_inlined_count,
                "compile passes complete"
            );
        }

        Ok(graph)
    }
}

// ─── Utilities ─────────────────────────────────────────────────

pub(crate) fn fnv_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 计算图结构 hash — 不依赖 HashMap 迭代顺序。
fn compute_structural_hash<S: WorkflowState, M: MergeStrategy<S>>(
    nodes: &IndexMap<String, NodeKind<S, M>>,
    edges: &[Edge<S>],
) -> u64 {
    let mut s = String::new();
    let mut names: Vec<&str> = nodes.keys().map(|k| k.as_str()).collect();
    names.sort();
    s.push_str(&names.join(","));
    s.push('|');
    let mut edge_strs: Vec<String> = edges
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
