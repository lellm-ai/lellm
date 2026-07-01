//! SubgraphSpec — Builder 阶段的强类型 Subgraph 描述。
//!
//! # 设计理念
//!
//! ```text
//! Builder 阶段：
//!   SubgraphSpec<Outer, Inner, M, Lens>  (强类型)
//!
//! 编译阶段：
//!   CompiledSubgraph<Outer>  (类型擦除 Inner/Lens/M)
//!
//! Engine 执行：
//!   match node.kind {
//!       NodeKind::Subgraph(spec) => self.execute_subgraph(spec).await,
//!   }
//! ```
//!
//! # 与 CompiledSubgraph 的区别
//!
//! - SubgraphSpec：Builder 阶段，强类型，包含 Graph + Lens
//! - CompiledSubgraph：编译后，类型擦除，可存入 NodeKind
//! - SubgraphSpec 实现 `StateProjector` trait，可转换为 CompiledSubgraph
//!
//! # 状态投影
//!
//! 通过 `StateLens` 从外层 State 投影出内层 State：
//!
//! ```text
//! WorkflowState
//!     ↓ StateLens
//! &mut AgentState
//!     ↓
//! Agent Graph 操作
//!     ↓ 借用结束
//! WorkflowState 继续
//! ```

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;

use crate::compiled_subgraph::{CompiledSubgraph, StateProjector};
use crate::error::GraphError;
use crate::graph::Graph;
use crate::state_lens::StateLens;
use crate::stream_emitter::StreamSink;
use crate::workflow_state::{MergeStrategy, WorkflowState};
use tokio_util::sync::CancellationToken;

/// Subgraph Builder 描述 — 强类型，包含 Graph + Lens。
///
/// # 泛型参数
///
/// - `Outer` — 外层 State 类型（如 WorkflowState）
/// - `Inner` — 内层 State 类型（如 AgentState）
/// - `M` — MergeStrategy 实现（用于 Graph）
/// - `L` — StateLens 实现，用于状态投影
///
/// # 使用方式
///
/// ```ignore
/// let spec = SubgraphSpec::new(agent_graph, AgentLens);
/// let compiled: CompiledSubgraph<WorkflowState> = spec.compile();
/// ```
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
where
    Outer: 'static,
    Inner: 'static,
    M: 'static,
    L: 'static,
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

    /// 编译为 CompiledSubgraph — 类型擦除 Inner/Lens/M。
    pub fn compile(self) -> CompiledSubgraph<Outer> {
        let max_steps = self.max_steps;
        CompiledSubgraph::new(Arc::new(self), max_steps)
    }
}

// ─── StateProjector 实现 ──────────────────────────────────────

impl<
    Outer: WorkflowState,
    Inner: WorkflowState,
    M: MergeStrategy<Inner>,
    L: StateLens<Outer, Inner>,
> StateProjector<Outer> for SubgraphSpec<Outer, Inner, M, L>
where
    Inner: 'static,
    M: 'static,
    L: 'static,
{
    /// 执行 Subgraph — 投影状态 + 递归执行内层 Graph。
    ///
    /// # 执行流程
    ///
    /// 1. 通过 Lens 投影出内层 State（`&mut Inner`）
    /// 2. 创建内层 ExecutionEngine（借用 `&mut Inner`）
    /// 3. 调用 `graph.run_inline()`
    /// 4. inner_engine drop → 借用释放 → outer 可继续使用
    fn execute<'a>(
        &'a self,
        outer: &'a mut Outer,
        stream: Option<Arc<dyn StreamSink>>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<(), GraphError>> + Send + 'a>> {
        Box::pin(async move {
            // 1. 通过 Lens 投影出内层 State
            let inner_ref = self.lens.get(outer);

            // 2. 创建内层 ExecutionEngine（借用 inner_ref）
            let mut inner_engine = crate::ExecutionEngine::new(inner_ref, stream, cancel);

            // 3. 执行内层 Graph
            self.graph
                .run_inline(&mut inner_engine, self.max_steps)
                .await?;

            // 4. inner_engine drop → 借用释放 → outer 可继续使用
            Ok(())
        })
    }

    fn graph_name(&self) -> &str {
        self.graph.name()
    }

    fn node_count(&self) -> usize {
        self.graph.node_names().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State;

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

        // 测试 Lens 投影
        let lens = TestLens;
        let inner = lens.get(&mut outer);

        // 修改 inner
        inner.value = 100;

        // 验证 outer.inner 被修改
        assert_eq!(outer.inner.value, 100);
    }
}
