//! InlinePass — Subgraph 内联优化 pass。

use super::context::CompilerContext;
use super::pass::CompilerPass;
use crate::Graph;
use crate::MergeStrategy;
use crate::node::NodeKind;
use crate::workflow_state::WorkflowState;

/// Subgraph 内联优化 pass。
///
/// 自动识别 SubgraphNode，评估是否值得内联，
/// 如果值得则展开 Subgraph，合并到外层 Graph。
///
/// # 设计理念
///
/// - 像 LLVM 的 function inlining
/// - 用户不应该手动调用 `builder.merge()`
/// - 应该由 `compile()` 自动决定
///
/// # 评估标准
///
/// 1. 图大小 < 阈值
/// 2. 没有外部依赖
/// 3. StateLens 是纯投影
pub struct InlinePass;

impl InlinePass {
    /// 创建新的 InlinePass。
    pub fn new() -> Self {
        Self
    }
}

impl Default for InlinePass {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: WorkflowState, M: MergeStrategy<S>> CompilerPass<S, M> for InlinePass {
    fn name(&self) -> &str {
        "inline"
    }

    fn run(&self, graph: &mut Graph<S, M>, ctx: &mut CompilerContext<S>) -> bool {
        // 1. 统计节点数
        ctx.stats.total_nodes_before = graph.nodes.len();

        // 2. 识别所有 Subgraph 节点
        let subgraph_nodes: Vec<String> = graph
            .nodes
            .iter()
            .filter(|(_, kind)| matches!(kind, NodeKind::Subgraph(_)))
            .map(|(name, _)| name.clone())
            .collect();

        ctx.stats.subgraph_count = subgraph_nodes.len();

        if ctx.debug {
            tracing::debug!(
                subgraph_count = subgraph_nodes.len(),
                "InlinePass: found subgraph nodes"
            );
        }

        // 3. 对每个 Subgraph 评估是否值得内联
        let modified = false;
        for node_name in subgraph_nodes {
            // 获取 Subgraph 节点
            if let Some(NodeKind::Subgraph(_subgraph)) = graph.nodes.get(&node_name) {
                // TODO: 评估是否值得内联
                // 暂时跳过，不内联
                ctx.stats.not_inlined_count += 1;
                if ctx.debug {
                    tracing::debug!(
                        node = %node_name,
                        "InlinePass: skipping subgraph (not worth inlining)"
                    );
                }
            }
        }

        // 4. 更新统计信息
        ctx.stats.total_nodes_after = graph.nodes.len();

        modified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphBuilder, NodeKind, State, StateMerge, TaskNode};

    #[test]
    fn test_inline_pass_no_subgraphs() {
        let mut builder = GraphBuilder::<State, StateMerge>::new("test");
        builder.start("a");
        builder.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        builder.end("a");
        let mut graph = builder.build().unwrap();

        let mut ctx = CompilerContext::new();
        let pass = InlinePass::new();

        let modified = pass.run(&mut graph, &mut ctx);

        assert!(!modified);
        assert_eq!(ctx.stats.subgraph_count, 0);
    }
}
