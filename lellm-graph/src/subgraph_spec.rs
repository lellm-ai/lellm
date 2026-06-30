//! SubgraphSpec — Subgraph 的编译后表示。
//!
//! Subgraph 不是 Node，而是 ExecutionEngine 的控制流概念。
//! SubgraphSpec 包含执行 Subgraph 所需的所有信息。
//!
//! # 设计理念
//!
//! ```text
//! Builder 阶段：
//!   builder.subgraph("agent", agent_graph, AgentLens)
//!
//! 编译后：
//!   CompiledNodeKind::Subgraph(SubgraphSpec { graph_id, lens })
//!
//! ExecutionEngine 执行时：
//!   match node.kind() {
//!       CompiledNodeKind::Subgraph(spec) => {
//!           self.execute_subgraph(spec).await;
//!       }
//!       // ...
//!   }
//! ```
//!
//! # 与 SubgraphNode 的区别
//!
//! - SubgraphNode 是 Builder AST 中的节点
//! - SubgraphSpec 是编译后的执行描述
//! - SubgraphSpec 由 ExecutionEngine 直接执行，不经过 Node::execute()

use std::marker::PhantomData;

use crate::Graph;
use crate::MergeStrategy;
use crate::state_lens::StateLens;
use crate::workflow_state::WorkflowState;

/// Subgraph 的编译后表示 — 由 ExecutionEngine 直接执行。
///
/// # 泛型参数
///
/// - `Outer` — 外层 State 类型（如 WorkflowState）
/// - `Inner` — 内层 State 类型（如 AgentState）
/// - `M` — MergeStrategy 实现（用于 Graph）
/// - `L` — StateLens 实现，用于状态投影
///
/// # 设计原则
///
/// - SubgraphSpec 不实现 Node trait
/// - ExecutionEngine 直接执行 SubgraphSpec
/// - 由 Engine 负责 Frame 管理、状态投影、Checkpoint 和恢复
pub struct SubgraphSpec<
    Outer: WorkflowState,
    Inner: WorkflowState,
    M: MergeStrategy<Inner>,
    L: StateLens<Outer, Inner>,
> {
    /// 内层 Graph
    pub graph: Graph<Inner, M>,

    /// 状态投影器
    pub lens: L,

    /// 最大执行步数
    pub max_steps: usize,

    /// PhantomData
    _phantom: PhantomData<Outer>,
}

impl<
    Outer: WorkflowState,
    Inner: WorkflowState,
    M: MergeStrategy<Inner>,
    L: StateLens<Outer, Inner>,
> SubgraphSpec<Outer, Inner, M, L>
{
    /// 创建新的 SubgraphSpec。
    ///
    /// # 参数
    ///
    /// - `graph` — 内层 Graph
    /// - `lens` — 状态投影器
    ///
    /// # 示例
    ///
    /// ```ignore
    /// let spec = SubgraphSpec::new(agent_graph, AgentLens);
    /// ```
    pub fn new(graph: Graph<Inner, M>, lens: L) -> Self {
        Self {
            graph,
            lens,
            max_steps: 1000, // 默认最大步数
            _phantom: PhantomData,
        }
    }

    /// 设置最大执行步数。
    pub fn max_steps(mut self, max: usize) -> Self {
        self.max_steps = max;
        self
    }

    /// 通过 Lens 投影状态。
    ///
    /// 从外层 State 投影出内层 State 的可变引用。
    pub fn project<'a>(&self, outer: &'a mut Outer) -> &'a mut Inner {
        self.lens.get(outer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::State;

    #[derive(Debug, PartialEq)]
    struct OuterState {
        inner: InnerState,
    }

    #[derive(Debug, PartialEq)]
    struct InnerState {
        value: i32,
    }

    struct TestLens;

    impl StateLens<OuterState, InnerState> for TestLens {
        fn get<'a>(&self, outer: &'a mut OuterState) -> &'a mut InnerState {
            &mut outer.inner
        }
    }

    #[test]
    fn test_subgraph_spec_projection() {
        let mut outer = OuterState {
            inner: InnerState { value: 42 },
        };

        // 创建一个简单的 Graph 用于测试
        // 注意：这里需要一个实际的 Graph，但为了测试我们只测试 projection
        // 实际测试需要完整的 Graph 构建

        // 测试 Lens 投影
        let lens = TestLens;
        let inner = lens.get(&mut outer);

        // 修改 inner
        inner.value = 100;

        // 验证 outer.inner 被修改
        assert_eq!(outer.inner.value, 100);
    }
}
