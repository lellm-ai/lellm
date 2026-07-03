//! InlinePass — Subgraph 内联优化 pass。

use super::context::CompilerContext;
use super::pass::CompilerPass;
use crate::Graph;
use crate::MergeStrategy;
use crate::node::NodeKind;
use crate::state::workflow_state::WorkflowState;

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
///
/// # 当前状态
///
/// ⚠️ **骨架实现** — 目前仅识别 Subgraph 节点并收集统计信息，
/// 不执行实际的内联展开。`run()` 始终返回 `false`。
/// 完整的内联逻辑需要处理：
/// - StateLens 的类型擦除与节点重映射
/// - 边重定向（外层 → 内层入口 / 内层出口 → 外层）
/// - NodeId 命名空间隔离
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
        //
        // ⚠️ TODO: 实现内联逻辑。当前为骨架，始终跳过。
        //    需要实现：
        //    a) 评估标准：图大小、外部依赖、Lens 类型
        //    b) 展开 Subgraph 节点到外层 Graph
        //    c) 重映射 NodeId 和边
        //    d) 更新 ctx.stats.inlined_count
        let mut modified = false;
        for node_name in subgraph_nodes {
            if let Some(NodeKind::Subgraph(_subgraph)) = graph.nodes.get(&node_name) {
                // 暂时跳过，不内联
                ctx.stats.not_inlined_count += 1;
                if ctx.debug {
                    tracing::debug!(
                        node = %node_name,
                        "InlinePass: skipping subgraph (inlining not yet implemented)"
                    );
                }
            }
            // 内联实现后，此处应修改 modified = true;
            let _ = &mut modified;
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
