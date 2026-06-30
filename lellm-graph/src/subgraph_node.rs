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
    /// 1. 从外层 State 通过 Lens 投影出内层 State
    /// 2. 创建内层 ExecutionContext
    /// 3. 调用 `graph.run_inline()`
    /// 4. 返回后借用自动释放
    ///
    /// # 借用安全
    ///
    /// `lens.get()` 返回的引用生命周期限制在本函数内，
    /// 退出后借用结束，外层 State 可继续使用。
    pub async fn execute(
        &self,
        outer: &mut Outer,
        _stream: Option<Arc<dyn crate::StreamSink>>,
        _cancellation: crate::CancellationToken,
    ) -> Result<(), crate::GraphError> {
        // 1. 通过 Lens 投影出内层 State
        let _inner = self.lens.get(outer);

        // 2. 创建内层 ExecutionContext
        // 注意：inner 是 &mut Inner，需要拥有所有权才能创建 ExecutionContext
        // 所以我们需要先 take 出来，执行完再放回去
        //
        // 但是 Inner 是 WorkflowState，不一定有 Default 实现
        // 所以这里有个设计问题...
        //
        // 解决方案：ExecutionContext 持有 &mut Inner 的引用
        // 但这需要 lifetime 参数，比较复杂
        //
        // 更简单的方案：SubgraphNode 直接操作 &mut Outer，
        // 通过 Lens 获取 Inner 的引用，传给 graph.run_inline()
        //
        // 但是 graph.run_inline() 需要 ExecutionContext<Inner>
        // ExecutionContext 需要拥有 Inner 的所有权
        //
        // 这是一个设计难题...

        // 暂时用简化方案：
        // SubgraphNode 不直接执行，而是返回一个闭包
        // 由 ExecutionEngine 负责执行
        //
        // 或者：SubgraphNode 实现 FlowNode trait
        // 在 FlowNode::execute() 中通过 NodeContext 获取 state

        todo!("SubgraphNode::execute() 需要重新设计")
    }
}

// ─── FlowNode 实现 ─────────────────────────────────────────────

// SubgraphNode 需要实现 FlowNode trait
// 但是 FlowNode::execute() 的签名是：
//   async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError>;
//
// NodeContext 持有 &mut S，我们可以从中获取 state
// 然后通过 Lens 投影出 Inner
//
// 问题是：ExecutionContext 需要 Inner 的所有权
// 但 NodeContext 只提供 &mut S 的引用
//
// 解决方案：
// 1. SubgraphNode 不实现 FlowNode
// 2. 在 ExecutionEngine 中特殊处理 SubgraphNode
// 3. 或者修改 ExecutionContext 支持引用语义

// 暂时标记为 todo
