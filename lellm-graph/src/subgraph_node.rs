//! SubgraphNode — 运行时递归执行 Subgraph。
//!
//! 用于组合多个 Graph，实现 Workflow → Agent → Tool 等多层架构。
//!
//! # 执行语义
//!
//! ```text
//! Workflow Graph
//!     ↓
//! SubgraphNode (agent)
//!     ↓ 递归调用
//! Agent Graph.run_inline()
//!     ↓ 返回
//! Workflow 继续执行
//! ```
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
//!
//! # 示例
//!
//! ```ignore
//! use lellm_graph::{SubgraphNode, StateLens, GraphBuilder, NodeKind};
//!
//! // 定义 State
//! struct WorkflowState {
//!     agent: AgentState,
//! }
//!
//! // 定义 Lens
//! struct AgentLens;
//!
//! impl StateLens<WorkflowState, AgentState> for AgentLens {
//!     fn get<'a>(&self, outer: &'a mut WorkflowState) -> &'a mut AgentState {
//!         &mut outer.agent
//!     }
//! }
//!
//! // 构建 Workflow
//! let agent_graph = AgentBuilder::new(model).tools([...]).build();
//!
//! let mut builder = GraphBuilder::<WorkflowState, _>::new("workflow");
//! builder.node(
//!     "agent",
//!     NodeKind::Subgraph(SubgraphNode::new(agent_graph, AgentLens)),
//! );
//!
//! // 执行
//! let graph = builder.build()?;
//! let mut ctx = ExecutionContext::new(workflow_state, None, CancellationToken::new());
//! graph.run_inline(&mut ctx, 100).await?;
//! ```

use std::marker::PhantomData;
use std::sync::Arc;

use crate::Graph;
use crate::MergeStrategy;
use crate::state_lens::StateLens;
use crate::workflow_state::WorkflowState;

/// Subgraph 节点 — 运行时递归执行内层 Graph。
///
/// # 泛型参数
///
/// - `Outer` — 外层 State 类型（如 WorkflowState）
/// - `Inner` — 内层 State 类型（如 AgentState）
/// - `M` — MergeStrategy 实现（用于 Graph）
/// - `L` — StateLens 实现，用于状态投影
///
/// # 设计理念
///
/// SubgraphNode 不持有 ExecutionContext，
/// 而是在执行时创建新的 ExecutionContext，
/// 通过 StateLens 投影状态。
///
/// 这样：
/// - 借用边界清晰
/// - 不需要 Frame Stack
/// - 只是递归函数调用
pub struct SubgraphNode<
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
> SubgraphNode<Outer, Inner, M, L>
{
    /// 创建新的 SubgraphNode。
    ///
    /// # 参数
    ///
    /// - `graph` — 内层 Graph
    /// - `lens` — 状态投影器
    ///
    /// # 示例
    ///
    /// ```ignore
    /// let node = SubgraphNode::new(agent_graph, AgentLens);
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

    /// 执行 Subgraph。
    ///
    /// # 执行流程
    ///
    /// 1. 从外层 State 通过 Lens 投影出内层 State（`&mut Inner`）
    /// 2. 创建内层 `ExecutionEngine<'_, Inner>`（借用 `&mut Inner`）
    /// 3. 调用 `graph.run_inline()`
    /// 4. inner_engine drop → 借用释放 → outer 可继续使用
    ///
    /// # 借用安全
    ///
    /// `lens.get()` 返回的引用生命周期限制在本函数内，
    /// 退出后借用结束，外层 State 可继续使用。
    /// Rust 的 borrow checker 在编译期保证状态安全。
    pub async fn execute(
        &self,
        outer: &mut Outer,
        stream: Option<Arc<dyn crate::StreamSink>>,
        cancellation: crate::CancellationToken,
    ) -> Result<(), crate::GraphError> {
        // 1. 通过 Lens 投影出内层 State
        let inner_ref = self.lens.get(outer);

        // 2. 创建内层 ExecutionEngine（借用 inner_ref）
        let mut inner_engine = crate::ExecutionEngine::new(inner_ref, stream, cancellation);

        // 3. 执行内层 Graph
        self.graph
            .run_inline(&mut inner_engine, self.max_steps)
            .await?;

        // 4. inner_engine drop → 借用释放 → outer 可继续使用
        Ok(())
    }
}

// ─── FlowNode 实现 ─────────────────────────────────────────────

// SubgraphNode 不实现 FlowNode。
// 它在 ExecutionEngine 的 match dispatch 中被特殊处理：
//   NodeKind::Subgraph(spec) => spec.execute(outer, stream, cancel).await
//
// 因为 SubgraphNode 需要 Outer 和 Inner 两种类型参数，
// 而 FlowNode<S> 只知道 S，无法表达 StateLens 投影。
