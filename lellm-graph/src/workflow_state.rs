//! WorkflowState + Mutation + MergeStrategy — Typed State 框架。
//!
//! v0.4+ 终局：砸碎 `HashMap<String, Value>`，引入编译期类型安全。
//!
//! 核心原则：
//! - 状态是强类型 struct，不是动态 HashMap
//! - 状态变更通过 Mutation（确定性命令），不是节点直接写
//! - Mutation 自己知道如何修改 State（CQRS / Event Sourcing 职责划分）
//! - 并行合并规则由 Graph 层的 MergeStrategy 决定，不是 State 内建属性
//! - Checkpoint 采用 Snapshot 实现快速恢复；Mutation Log 用于审计、调试和可选的
//!   确定性重放，两者共同构成完整的恢复与追踪体系
//!
//! Graph 层提供 trait 框架，各业务层（agent/mcp/...）定义自己的 State + Mutation。

use std::fmt::Debug;

// ─── StateMutation ──────────────────────────────────────────────

/// 状态变更命令 — 描述一次对 State 的确定性修改。
///
/// Mutation 自己知道如何修改对应的 State（`apply(self, &mut S)`）。
/// State 只是数据，Mutation 是变更逻辑，Executor 负责调度。
///
/// # 设计原则
///
/// - **Command 而非 Patch**：`AppendMessage` 而非 `SetMessages`
/// - **Enum 分发**：顶层 enum 只做一层 match，具体逻辑在各 variant 的 `apply()` 中
/// - **无 Serialize 强制**：只有需要 Replay 的运行时才加 `Serialize` bound
pub trait StateMutation<S>: Sized + Send + Sync + Debug {
    /// 将此 Mutation 应用到目标 State。
    ///
    /// 这是 Mutation 的核心职责：每个 variant 独立实现，
    /// 顶层 enum 只做一层 dispatch。
    fn apply(self, state: &mut S);

    /// 将此 Mutation 合并到另一个同类型 Mutation 中（可选）。
    ///
    /// 用于批量场景：多个 Mutation 合并为一个，减少 apply 次数。
    /// 默认返回 `None` 表示不可合并。
    fn combine(self, _other: Self) -> Option<Self> {
        None
    }
}

// ─── WorkflowState ──────────────────────────────────────────────

/// 工作流状态 — 编译期类型安全的状态容器。
///
/// 替代 `HashMap<String, Value>` 动态模型。
/// 每个工作流定义自己的 State struct 和 Mutation enum，
/// 实现此 trait 以声明关联类型。
///
/// **State 只是数据。** 状态变更逻辑在 [`StateMutation`] trait 中。
/// **Merge 职责已从 `WorkflowState` 剥离到 [`MergeStrategy`]。**
/// **Checkpoint 采用 Projection 模式** — Runtime State 可包含不可序列化字段
/// （如 `Arc<dyn ...>`, `Sender`, `Cache`），Checkpoint 只序列化必要字段。
///
/// # 示例
///
/// ```rust,ignore
/// // State 只是数据
/// pub struct AgentState {
///     pub messages: Vec<Message>,
///     pub iterations: usize,
///     pub output_tokens: usize,
///     pub cache: Arc<dyn ToolCatalog>,  // 不可序列化
/// }
///
/// // 可序列化的 Checkpoint 投影
/// #[derive(Serialize, Deserialize)]
/// pub struct AgentCheckpoint {
///     pub messages: Vec<Message>,
///     pub iterations: usize,
///     pub output_tokens: usize,
///     // 不包含 cache
/// }
///
/// // Mutation 自己知道怎么改 State
/// pub enum AgentMutation {
///     AppendMessage(Message),
///     IncrementIteration,
///     RecordOutputTokens(usize),
/// }
///
/// impl StateMutation<AgentState> for AgentMutation {
///     fn apply(self, state: &mut AgentState) {
///         match self {
///             AgentMutation::AppendMessage(msg) => state.messages.push(msg),
///             AgentMutation::IncrementIteration => state.iterations += 1,
///             AgentMutation::RecordOutputTokens(n) => state.output_tokens += n,
///         }
///     }
/// }
///
/// // WorkflowState 声明 Checkpoint 和 Mutation 关联类型
/// impl WorkflowState for AgentState {
///     type Checkpoint = AgentCheckpoint;
///     type Mutation = AgentMutation;
///
///     fn snapshot(&self) -> AgentCheckpoint {
///         AgentCheckpoint {
///             messages: self.messages.clone(),
///             iterations: self.iterations,
///             output_tokens: self.output_tokens,
///         }
///     }
///
///     fn restore(checkpoint: AgentCheckpoint) -> Self {
///         AgentState {
///             messages: checkpoint.messages,
///             iterations: checkpoint.iterations,
///             output_tokens: checkpoint.output_tokens,
///             cache: Arc::new(ToolCatalog::default()),  // 重建
///         }
///     }
/// }
/// ```
pub trait WorkflowState: Clone + Send + Sync {
    /// 可序列化的 Checkpoint 快照（projection，不是 raw state）。
    ///
    /// Runtime State 可以包含不可序列化字段（`Arc<dyn ...>`, `Sender`, `Cache`），
    /// Checkpoint 只序列化必要字段。这是强制的 Projection 模式。
    type Checkpoint: serde::Serialize + serde::de::DeserializeOwned + Clone + Send;

    /// 与此状态关联的 Mutation 类型。
    type Mutation: StateMutation<Self>;

    /// 创建 checkpoint 快照 — 只序列化必要字段。
    ///
    /// 这是 Projection 的核心：开发者必须决定哪些字段需要持久化。
    /// 编译期保证 `Checkpoint` 可序列化。
    fn snapshot(&self) -> Self::Checkpoint;

    /// 从 checkpoint 恢复运行时状态。
    ///
    /// 恢复时明确哪些字段从 checkpoint 加载，哪些需要重建。
    fn restore(checkpoint: Self::Checkpoint) -> Self;

    /// 批量应用 Mutation — 唯一公开入口。
    ///
    /// 默认实现：逐个调用 [`StateMutation::apply`]。
    /// 未来可覆盖为 Transaction 语义（begin → validate → apply → commit/rollback）。
    fn apply_batch(&mut self, mutations: impl IntoIterator<Item = Self::Mutation>) {
        for mutation in mutations {
            mutation.apply(self);
        }
    }

    /// 创建默认/初始状态。
    fn initial() -> Self
    where
        Self: Default,
    {
        Self::default()
    }
}

// ─── MergeStrategy ──────────────────────────────────────────────

/// 并行分支合并策略 — Graph 层职责，非 State 内建属性。
///
/// 将多个并行分支执行后产生的状态合并为一个。
/// 合并规则由 Graph 编排层决定，而非 State 自身。
///
/// # 职责边界
///
/// - **State** = 数据
/// - **MergeStrategy** = 并行语义
/// - **ExecutionEngine** = 调度 + commit
/// - **Node** = Mutation Producer
///
/// # 示例
///
/// ```rust,ignore
/// // 为 AgentState 定义合并策略
/// pub struct AgentStateMerge;
/// impl MergeStrategy<AgentState> for AgentStateMerge {
///     fn merge(branches: Vec<AgentState>) -> Result<AgentState, WorkflowError> {
///         // messages: concat, iterations: max, tokens: sum
///     }
/// }
///
/// // ParallelNode 使用
/// ParallelNode::builder()
///     .merge_strategy(AgentStateMerge)
///     .branch("search", search_node)
///     .branch("analyze", analyze_node)
///     .build();
/// ```
pub trait MergeStrategy<S>: Send + Sync {
    /// 合并多个并行分支的状态。
    ///
    /// `branches` 按注册顺序排列（与 ParallelNode 的 branch 注册顺序一致）。
    fn merge(branches: Vec<S>) -> Result<S, WorkflowError>;

    /// 创建策略的默认实例（供 ParallelNodeBuilder 使用）。
    /// 对于无状态策略（如 StateMerge、LastWriteWins），直接返回自身。
    fn default_instance() -> Self;
}

/// 默认合并策略 — 最后一个分支获胜。
///
/// 适用于大多数场景：各分支从同一 base 出发，
/// 最后一个分支的写入覆盖前面的。
pub struct LastWriteWins;

impl<S> MergeStrategy<S> for LastWriteWins {
    fn merge(branches: Vec<S>) -> Result<S, WorkflowError> {
        branches
            .into_iter()
            .last()
            .ok_or_else(|| WorkflowError::MergeConflict("no branches to merge".into()))
    }

    fn default_instance() -> Self {
        LastWriteWins
    }
}

// ─── WorkflowError ──────────────────────────────────────────────

/// 工作流状态操作错误。
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// 状态合并冲突
    #[error("state merge conflict: {0}")]
    MergeConflict(String),

    /// Mutation 应用失败
    #[error("failed to apply mutation: {0}")]
    ApplyFailed(String),

    /// 状态序列化/反序列化失败
    #[error("state serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
