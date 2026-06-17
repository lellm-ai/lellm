//! ParallelNode — 并行执行多个分支，合并 StateDelta。
//!
//! 执行模型：
//! ```text
//! State
//!  ↓
//! fork (ParallelNode)
//!  ↓
//! Branch A     Branch B     Branch C
//!  ↓            ↓            ↓
//! StateDelta   StateDelta   StateDelta
//!  ↓            ↓            ↓
//! ReducerRegistry.merge_deltas()
//!  ↓
//! Merged Deltas → apply to State
//! ```
//!
//! 每个分支接收相同的 State 快照，独立产生 StateDelta。
//! 所有 Delta 收集后通过 `ReducerRegistry::merge_deltas()` 合并。
//! 未注册 Reducer 的 key 发生多 writer → `StateConflict` 错误。

use std::sync::Arc;

use crate::error::GraphError;
use crate::node::{FlowNode, NextStep, NodeOutput};
use crate::state::State;

/// 并行节点 — 同时执行多个分支，合并 StateDelta。
///
/// 每个分支接收相同的 State 快照，独立产生 StateDelta。
/// 所有分支完成后，Delta 通过 `ReducerRegistry::merge_deltas()` 合并到 State。
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
#[derive(Clone)]
pub struct ParallelNode {
    /// 调试标签（可选）
    label: Option<String>,
    /// 并行分支 — (名称, 节点)
    branches: Vec<(String, Arc<dyn FlowNode>)>,
    /// 错误处理策略
    error_strategy: ParallelErrorStrategy,
}

/// 并行执行错误处理策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParallelErrorStrategy {
    /// 任一分支失败 → 立即返回错误（其余分支继续执行但结果被忽略）
    #[default]
    FailFast,
    /// 等待所有分支完成，至少一个失败 → 返回错误但包含成功分支的 Delta
    CollectAll,
}

impl ParallelNode {
    /// 创建构建器。
    pub fn builder() -> ParallelNodeBuilder {
        ParallelNodeBuilder::new()
    }

    /// 设置调试标签。
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 获取分支数量。
    pub fn branch_count(&self) -> usize {
        self.branches.len()
    }

    /// 获取分支名称列表。
    pub fn branch_names(&self) -> Vec<&str> {
        self.branches
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// 迭代所有分支（名称, 节点）引用。
    pub fn branches_iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn FlowNode>)> {
        self.branches
            .iter()
            .map(|(name, node)| (name.as_str(), node))
    }

    /// 获取错误处理策略。
    pub fn error_strategy(&self) -> ParallelErrorStrategy {
        self.error_strategy
    }

    /// 获取标签。
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// 串行执行所有分支（用于阻塞模式 fallback）。
    ///
    /// ⚠️ 此方法顺序执行各分支，不发挥并行优势。
    /// 真正的并行执行由 `Executor::handle_parallel()` 完成。
    pub async fn execute_sequential(&self, state: &State) -> Result<NodeOutput, GraphError> {
        let mut all_deltas = Vec::new();

        for (name, branch) in &self.branches {
            let output = branch.execute(state).await.map_err(|e| {
                GraphError::Terminal(crate::error::TerminalError::NodeExecutionFailed {
                    node: format!("{}/{}", self.display_name(), name),
                    source: e.into(),
                })
            })?;
            all_deltas.extend(output.deltas);
        }

        Ok(NodeOutput {
            deltas: all_deltas,
            next: NextStep::GoToNext,
            metadata: None,
        })
    }

    fn display_name(&self) -> String {
        self.label.clone().unwrap_or_else(|| "parallel".to_string())
    }
}

/// ParallelNode 构建器。
pub struct ParallelNodeBuilder {
    label: Option<String>,
    branches: Vec<(String, Arc<dyn FlowNode>)>,
    error_strategy: ParallelErrorStrategy,
}

impl ParallelNodeBuilder {
    fn new() -> Self {
        Self {
            label: None,
            branches: Vec::new(),
            error_strategy: ParallelErrorStrategy::default(),
        }
    }

    /// 设置调试标签。
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 添加并行分支。
    ///
    /// - `name` — 分支名称（用于调试和事件标识）
    /// - `node` — 分支执行的节点
    pub fn branch(mut self, name: impl Into<String>, node: Arc<dyn FlowNode>) -> Self {
        self.branches.push((name.into(), node));
        self
    }

    /// 设置错误处理策略。
    pub fn error_strategy(mut self, strategy: ParallelErrorStrategy) -> Self {
        self.error_strategy = strategy;
        self
    }

    /// 构建 ParallelNode。
    ///
    /// # Panics
    ///
    /// 如果没有添加任何分支，则 panic。
    pub fn build(self) -> ParallelNode {
        if self.branches.is_empty() {
            panic!("ParallelNode must have at least one branch");
        }
        ParallelNode {
            label: self.label,
            branches: self.branches,
            error_strategy: self.error_strategy,
        }
    }
}

impl std::fmt::Debug for ParallelNode {
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
