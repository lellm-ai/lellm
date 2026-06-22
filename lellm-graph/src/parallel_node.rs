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
use crate::node_context::NodeContext;
use crate::state::{State, StateMerge};
use crate::workflow_state::{MergeStrategy, WorkflowState};

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
    merge_strategy: M,
}

impl<S: WorkflowState, M: MergeStrategy<S>> Clone for ParallelNode<S, M> {
    fn clone(&self) -> Self {
        Self {
            label: self.label.clone(),
            branches: self.branches.clone(),
            error_strategy: self.error_strategy,
            merge_strategy: M::default_instance(),
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
    merge_strategy: M,
}

impl<S: WorkflowState, M: MergeStrategy<S>> ParallelNodeBuilder<S, M> {
    fn new() -> Self
    where
        M: Sized,
    {
        Self {
            label: None,
            branches: Vec::new(),
            error_strategy: ParallelErrorStrategy::default(),
            merge_strategy: M::default_instance(),
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
            merge_strategy: self.merge_strategy,
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
            merge_strategy: NM::default_instance(),
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
        let mut branch_results: Vec<S> = Vec::with_capacity(self.branches.len());

        // Execute branches sequentially (serial fallback)
        for (name, node) in &self.branches {
            let branch_start = Instant::now();
            let branch_span = SpanId::new();

            // Each branch gets its own typed state clone + a forked BranchState
            let mut branch_state = base_state.clone();
            let mut branch_bs = ctx.branch().fork();
            let mut branch_ctx = NodeContext::new(&mut branch_state, &mut branch_bs, None);

            let result = node.execute(&mut branch_ctx).await.map_err(|e| {
                GraphError::Terminal(crate::error::TerminalError::NodeExecutionFailed {
                    node: format!("{}/{}", self.display_name(), name),
                    source: e.into(),
                })
            });

            // Consume effects → apply to branch's typed state
            let effects = branch_ctx.consume_effects();
            for v in effects {
                if let Ok(effect) = serde_json::from_value::<S::Effect>(v) {
                    branch_state.apply(effect);
                }
            }

            let branch_duration = branch_start.elapsed();
            let success = result.is_ok();

            ctx.emit_flow_event(FlowEvent::BranchCompleted {
                branch_name: name.clone(),
                node_id: self.display_name(),
                span_id: branch_span,
                success,
                duration: branch_duration,
            });

            if !success {
                return result;
            }

            branch_results.push(branch_state);
        }

        // Merge all branch states using MergeStrategy — Graph 层并行语义
        let merged = M::merge(branch_results).map_err(|e| {
            GraphError::Terminal(crate::error::TerminalError::StateError(format!(
                "parallel merge conflict: {e}",
            )))
        })?;

        // Replace parent state with merged result
        *ctx.state_mut() = merged;

        ctx.emit_flow_event(FlowEvent::ParallelCompleted {
            node_id: self.display_name(),
            span_id,
            duration: start_time.elapsed(),
        });

        Ok(())
    }
}
