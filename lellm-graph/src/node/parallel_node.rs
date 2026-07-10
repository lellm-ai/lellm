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

use super::FlowNode;
use crate::error::GraphError;
use crate::exec::execution_engine::{ExecutionEngine, ExecutorState, OwnedExecutionEngine};
use crate::state::workflow_state::{MergeStrategy, WorkflowState};
use crate::state::{State, StateMerge};

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

impl<S: WorkflowState + Clone + Send + Sync, M: MergeStrategy<S>> ParallelNode<S, M> {
    /// 执行并行分支 — 创建独立的 OwnedExecutionEngine 给每个分支。
    pub async fn execute(&self, engine: &mut ExecutionEngine<'_, S>) -> Result<(), GraphError> {
        let start_time = Instant::now();
        let branch_count = self.branches.len();
        let display_name = self.display_name();

        tracing::debug!(
            parallel = %display_name,
            branches = branch_count,
            "parallel node started"
        );

        // Clone typed state for each branch — each branch works on its own copy
        let base_state = engine.clone_state();

        // Inherit parent's cancel token and stream (fan-out via Arc clone)
        let parent_cancel = engine.cancel_token().clone();
        let parent_stream = engine.stream_sink();

        // Clone branch data so async blocks own everything they need
        let branches: Vec<(String, Arc<dyn super::FlowNode<S>>)> = self
            .branches
            .iter()
            .map(|(n, nd)| (n.clone(), nd.clone()))
            .collect();

        // Create a future for each branch — no spawn, no 'static required
        let branch_futures: Vec<_> = branches
            .into_iter()
            .map(|(branch_name, node)| {
                let state = base_state.clone();
                let child_cancel = parent_cancel.child_token();
                let child_stream = parent_stream.clone();
                async move {
                    let branch_start = Instant::now();

                    // Each branch gets its own OwnedExecutionEngine (child engine)
                    let mut child_engine =
                        OwnedExecutionEngine::new(state, child_stream, child_cancel);

                    let mut branch_ctx = child_engine.build_node_context();
                    let ok = node.execute(&mut branch_ctx).await.is_ok();
                    // branch_ctx goes out of scope here; explicit drop removed
                    // (NodeContext only holds references, drop() is a no-op)

                    if !ok {
                        return (branch_name, Err("branch execution failed".into()));
                    }

                    // Commit mutations to child engine
                    child_engine.commit();

                    let duration = branch_start.elapsed();

                    (branch_name, Ok((child_engine.into_state(), duration)))
                }
            })
            .collect();

        // Execute all branches concurrently (no spawn, just concurrent polling)
        type BranchResult<S> = (String, Result<(S, std::time::Duration), String>);
        let raw_results: Vec<BranchResult<S>> = futures::future::join_all(branch_futures).await;

        // Process results in branch order
        let mut branch_states: Vec<S> = Vec::with_capacity(branch_count);
        let mut errors: Vec<(String, String)> = Vec::new();

        for (branch_name, result) in raw_results {
            match result {
                Ok((state, branch_duration)) => {
                    tracing::debug!(
                        parallel = %display_name,
                        branch = %branch_name,
                        duration_ms = branch_duration.as_millis(),
                        "branch completed"
                    );
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

        // Replace parent state with merged result
        engine.replace_state(merged);

        tracing::debug!(
            parallel = %display_name,
            duration_ms = start_time.elapsed().as_millis(),
            "parallel node completed"
        );

        Ok(())
    }
}
