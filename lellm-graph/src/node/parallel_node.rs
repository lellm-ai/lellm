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

#[allow(deprecated)]
use super::FlowNode;
use crate::error::GraphError;
use crate::exec::execution_engine::ExecutionEngine;
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
#[allow(deprecated)]
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
    /// 首个失败立即请求取消其余分支，等待协作退出后返回首个错误。
    #[default]
    FailFast,
    /// 等待所有分支完成并返回聚合错误；成功结果保留，父状态不部分提交。
    CollectAll,
}

impl ParallelNode {
    /// 创建默认构建器（`State` + `StateMerge`）。
    pub fn builder() -> ParallelNodeBuilder {
        ParallelNodeBuilder::new()
    }
}

#[allow(deprecated)]
impl<S: WorkflowState, M: MergeStrategy<S>> ParallelNode<S, M> {
    pub(crate) fn validate_approval_handlers(&self) -> Result<(), GraphError> {
        for (_, node) in &self.branches {
            node.validate_approval_handlers()?;
        }
        Ok(())
    }

    pub(crate) fn validate_durable(&self) -> Result<(), GraphError> {
        let mut names = std::collections::HashSet::new();
        for (name, node) in &self.branches {
            if !names.insert(name) {
                return Err(crate::durable::DurableError::Conflict(format!(
                    "duplicate branch name {name}"
                ))
                .into());
            }
            node.validate_durable()?;
        }
        Ok(())
    }

    pub(crate) fn durable_signature(&self) -> String {
        serde_json::to_string(&(
            format!("{:?}", self.error_strategy),
            self.branches
                .iter()
                .map(|(name, node)| (name, node.durable_signature()))
                .collect::<Vec<_>>(),
        ))
        .unwrap()
    }
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

    #[allow(deprecated)]
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
}

/// ParallelNode 构建器。
#[allow(deprecated)]
pub struct ParallelNodeBuilder<S: WorkflowState = State, M: MergeStrategy<S> = StateMerge> {
    label: Option<String>,
    branches: Vec<(String, Arc<dyn FlowNode<S>>)>,
    error_strategy: ParallelErrorStrategy,
    _phantom: std::marker::PhantomData<M>,
}

impl<S: WorkflowState, M: MergeStrategy<S>> ParallelNodeBuilder<S, M> {
    pub fn new() -> Self {
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

    #[allow(deprecated)]
    pub fn branch(mut self, name: impl Into<String>, node: Arc<dyn FlowNode<S>>) -> Self {
        self.branches.push((name.into(), node));
        self
    }

    /// 添加支持嵌套审批与恢复的图分支；局部预算之外仍受根运行总预算限制。
    pub fn graph_branch<IM>(
        mut self,
        name: impl Into<String>,
        graph: Arc<crate::Graph<S, IM>>,
        max_steps: usize,
    ) -> Self
    where
        S: 'static,
        IM: MergeStrategy<S> + 'static,
    {
        self.branches.push((
            name.into(),
            Arc::new(super::graph_branch::GraphBranch { graph, max_steps }),
        ));
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

impl<S: WorkflowState, M: MergeStrategy<S>> ParallelNode<S, M> {
    /// 并行执行；完成结果按声明顺序合并，错误时不修改父状态。
    pub fn execute<'a>(
        &'a self,
        engine: &'a mut ExecutionEngine<'_, S>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), GraphError>> + Send + 'a>>
    {
        Box::pin(super::parallel_execute::execute(self, engine))
    }
}
