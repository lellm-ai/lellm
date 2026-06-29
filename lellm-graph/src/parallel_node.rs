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
use crate::node_context::{ExecutionContext, ExecutorState, NodeContext};
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

    pub fn branches_iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn FlowNode<S>>)>{
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
impl<S: WorkflowState + Clone + Send + Sync, M: MergeStrategy<S>> FlowNode<S>
    for ParallelNode<S, M>
{
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
        let display_name = self.display_name();

        // Clone branch data so async blocks own everything they need
        let branches: Vec<(String, Arc<dyn FlowNode<S>>)> =
            self.branches.iter().map(|(n, nd)| (n.clone(), nd.clone())).collect();

        // Create a future for each branch — no spawn, no 'static required
        let branch_futures: Vec<_> = branches
            .into_iter()
            .map(|(branch_name, node)| {
                let state = base_state.clone();
                async move {
                    let branch_start = Instant::now();

                    let mut exec_ctx = ExecutionContext::new(
                        state,
                        None,
                        CancellationToken::new(),
                    );

                    let mut branch_ctx = exec_ctx.build_node_context();
                    let ok = node.execute(&mut branch_ctx).await.is_ok();
                    drop(branch_ctx);

                    if !ok {
                        return (branch_name, Err("branch execution failed".into()));
                    }

                    let mutations = exec_ctx.take_mutations();
                    exec_ctx.apply_batch(mutations);

                    let duration = branch_start.elapsed();

                    (branch_name, Ok((exec_ctx.into_state(), duration)))
                }
            })
            .collect();

        // Execute all branches concurrently (no spawn, just concurrent polling)
        let raw_results: Vec<(String, Result<(S, std::time::Duration), String>)> =
            futures::future::join_all(branch_futures).await;

        // Process results in branch order
        let mut branch_states: Vec<S> = Vec::with_capacity(branch_count);
        let mut errors: Vec<(String, String)> = Vec::new();

        for (branch_name, result) in raw_results {
            match result {
                Ok((state, branch_duration)) => {
                    ctx.emit_flow_event(FlowEvent::BranchCompleted {
                        branch_name,
                        node_id: display_name.clone(),
                        span_id: SpanId::new(),
                        success: true,
                        duration: branch_duration,
                    });
                    branch_states.push(state);
                }
                Err(reason) => {
                    errors.push((branch_name, reason));
                }
            }
        }

        // Error handling based on strategy
        if !errors.is_empty() {
            match self.error_strategy {
                ParallelErrorStrategy::FailFast => {
                    let (name, reason) = &errors[0];
                    return Err(GraphError::Terminal(
                        crate::error::TerminalError::NodeExecutionFailed {
                            node: format!("{}/{}", display_name, name),
                            source: reason.clone().into(),
                        },
                    ));
                }
                ParallelErrorStrategy::CollectAll => {
                    // CollectAll: wait for all branches, merge successful ones,
                    // but still return error if any branch failed.
                    if !branch_states.is_empty() {
                        for (name, reason) in &errors {
                            tracing::warn!(
                                parallel = %display_name,
                                branch = %name,
                                error = %reason,
                                "branch failed (CollectAll strategy)"
                            );
                        }
                    }
                    let (name, reason) = &errors[0];
                    return Err(GraphError::Terminal(
                        crate::error::TerminalError::NodeExecutionFailed {
                            node: format!("{}/{}", display_name, name),
                            source: reason.clone().into(),
                        },
                    ));
                }
            }
        }

        // Merge all branch states using MergeStrategy — Graph 层并行语义
        let merged = M::merge(branch_states).map_err(|e| {
            GraphError::Terminal(crate::error::TerminalError::StateError(format!(
                "parallel merge conflict: {e}",
            )))
        })?;

        // Replace parent state with merged result (sanctioned composite-node API)
        ctx.replace_state(merged);

        ctx.emit_flow_event(FlowEvent::ParallelCompleted {
            node_id: display_name,
            span_id,
            duration: start_time.elapsed(),
        });

        Ok(())
    }
}
