//! ParallelNode — 并行执行多个分支，通过 MergeStrategy 合并 State。
//!
//! 执行模型：
//! ```text
//! State
//!  ↓
//! fork (ParallelNode)
//!  ↓
//! Branch A     Branch B     Branch C
//!  ↓            ↓            ↓
//! State<S>     State<S>     State<S>
//!  ↓            ↓            ↓
//! MergeStrategy<S>::merge(branches)
//!  ↓
//! Merged State → replace parent state
//! ```
//!
//! 每个分支接收相同的 State 快照，独立产生变更（通过 Effects）。
//! 所有分支完成后，变更通过 MergeStrategy 合并到 State。

use std::sync::Arc;
use std::time::Instant;

use crate::error::GraphError;
use crate::event::FlowEvent;
use crate::ids::SpanId;
use crate::node::FlowNode;
use crate::node_context::{ExecutionContext, NodeContext};
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};
use tokio_util::sync::CancellationToken;

/// 并行节点 — 同时执行多个分支，通过 MergeStrategy 合并 State。
///
/// 每个分支接收相同的 State 快照，独立产生变更。
/// 所有分支完成后，变更通过 MergeStrategy 合并。
///
/// # 泛型参数
///
/// - `S` — 类型化状态
/// - `M` — 合并策略（默认为 [`StateMerge`]）
///
/// # 示例
///
/// ```rust,ignore
/// let parallel = ParallelNode::builder()
///     .branch("search", Arc::new(SearchNode::new()))
///     .branch("analyze", Arc::new(AnalyzeNode::new()))
///     .build();
///
/// graph.node("research", NodeKind::Parallel(parallel));
/// ```
pub struct ParallelNode<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    label: Option<String>,
    branches: Vec<(String, Arc<dyn FlowNode<S>>)>,
    error_strategy: ParallelErrorStrategy,
    /// Phantom — M 通过 `M::merge()` 静态调用，不需要实例。
    _merge_strategy: std::marker::PhantomData<M>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> Clone for ParallelNode<S, M> {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            branches: self.branches.clone(),
            error_strategy: self.error_strategy,
            _merge_strategy: std::marker::PhantomData,
        }
    }
}

/// 并行执行错误处理策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParallelErrorStrategy {
    /// 任一分支失败 → 立即返回错误（其余分支继续执行但结果被忽略）
    #[default]
    FailFast,
    /// 等待所有分支完成，至少一个失败 → 返回错误但包含成功分支的变更
    CollectAll,
}

impl ParallelNode {
    /// 创建默认构建器（`State` + `StateMerge`）。
    pub fn builder() -> ParallelNodeBuilder {
        ParallelNodeBuilder::new()
    }
}

impl<S: WorkflowState, M: MergeStrategy<S>> ParallelNode<S, M> {
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn branch_count(&self) -> usize {
        self.branches.len()
    }

    pub fn branch_names(&self) -> Vec<&str> {
        self.branches
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    pub fn branches_iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn FlowNode<S>>)> {
        self.branches
            .iter()
            .map(|(name, node)| (name.as_str(), node))
    }

    pub fn error_strategy(&self) -> ParallelErrorStrategy {
        self.error_strategy
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    fn display_name(&self) -> String {
        self.label.clone().unwrap_or_else(|| "parallel".to_string())
    }
}

/// ParallelNode 构建器。
pub struct ParallelNodeBuilder<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    label: Option<String>,
    branches: Vec<(String, Arc<dyn FlowNode<S>>)>,
    error_strategy: ParallelErrorStrategy,
    _phantom: std::marker::PhantomData<M>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> ParallelNodeBuilder<S, M> {
    fn new() -> Self {
        Self {
            label: None,
            branches: Vec::new(),
            error_strategy: ParallelErrorStrategy::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn branch(mut self, name: impl Into<String>, node: Arc<dyn FlowNode<S>>) -> Self {
        self.branches.push((name.into(), node));
        self
    }

    pub fn error_strategy(mut self, strategy: ParallelErrorStrategy) -> Self {
        self.error_strategy = strategy;
        self
    }

    pub fn build(self) -> ParallelNode<S, M> {
        if self.branches.is_empty() {
            panic!("ParallelNode must have at least one branch");
        }
        ParallelNode {
            label: self.label,
            branches: self.branches,
            error_strategy: self.error_strategy,
            _merge_strategy: std::marker::PhantomData,
        }
    }

    /// 替换合并策略，返回新类型的构建器。
    pub fn merge_strategy<NM>(self) -> ParallelNodeBuilder<S, NM>
    where
        NM: MergeStrategy<S>,
    {
        ParallelNodeBuilder {
            label: self.label,
            branches: self.branches,
            error_strategy: self.error_strategy,
            _phantom: std::marker::PhantomData,
        }
    }
}

/// 带 MergeStrategy 的构建器 — 由 ParallelNode::builder() 返回。
pub struct ParallelNodeBuilderWithMerge<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge>(
    pub ParallelNodeBuilder<S, M>,
);

impl<S: WorkflowState, M: MergeStrategy<S>> std::fmt::Debug for ParallelNode<S, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelNode")
            .field("label", &self.label)
            .field(
                "branches",
                &self
                    .branches
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("error_strategy", &self.error_strategy)
            .finish()
    }
}

#[async_trait::async_trait]
impl<S: WorkflowState, M: MergeStrategy<S>> FlowNode<S> for ParallelNode<S, M> {
    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        let start_time = Instant::now();
        let span_id = SpanId::new();
        let branch_count = self.branches.len();

        ctx.emit_flow_event(FlowEvent::ParallelStarted {
            node_id: self.display_name(),
            branch_count,
            span_id,
        });

        // Clone typed state for each branch — each branch works on its own copy
        let base_state = ctx.state().clone();
        let base_branch = ctx.fork_branch();
        let mut branch_results: Vec<S> = Vec::with_capacity(self.branches.len());

        // Execute branches sequentially (serial fallback)
        for (name, node) in &self.branches {
            let branch_start = Instant::now();
            let branch_span = SpanId::new();

            // Each branch gets its own ExecutionContext with cloned state
            let mut exec_ctx = ExecutionContext::new(
                base_state.clone(),
                base_branch.fork(),
                None,
                CancellationToken::new(),
            );

            let mut branch_ctx = exec_ctx.build_node_context();
            let branch_ok = node.execute(&mut branch_ctx).await.is_ok();
            drop(branch_ctx);

            if !branch_ok {
                return Err(GraphError::Terminal(
                    crate::error::TerminalError::NodeExecutionFailed {
                        node: format!("{}/{}", self.display_name(), name),
                        source: "branch execution failed".into(),
                    },
                ));
            }

            let mutations = exec_ctx.take_mutations();
            exec_ctx.state_mut().apply_batch(mutations);

            let branch_duration = branch_start.elapsed();

            ctx.emit_flow_event(FlowEvent::BranchCompleted {
                branch_name: name.clone(),
                node_id: self.display_name(),
                span_id: branch_span,
                success: true,
                duration: branch_duration,
            });

            branch_results.push(exec_ctx.state().clone());
        }

        // Merge all branch states using MergeStrategy — Graph 层并行语义
        let merged = M::merge(branch_results).map_err(|e| {
            GraphError::Terminal(crate::error::TerminalError::StateError(format!(
                "parallel merge conflict: {e}",
            )))
        })?;

        // Replace parent state with merged result
        ctx.replace_state(merged);

        ctx.emit_flow_event(FlowEvent::ParallelCompleted {
            node_id: self.display_name(),
            span_id,
            duration: start_time.elapsed(),
        });

        Ok(())
    }
}
