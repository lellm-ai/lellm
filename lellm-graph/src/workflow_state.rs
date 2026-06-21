//! WorkflowState + Effect — Typed State 框架。
//!
//! v0.4+ 终局：砸碎 `HashMap<String, Value>`，引入编译期类型安全。
//!
//! 核心原则：
//! - 状态是强类型 struct，不是动态 HashMap
//! - 状态变更通过 Effect（领域事件），不是节点直接写
//! - 合并规则在编译期确定，不靠运行时 ReducerRegistry
//! - Checkpoint = Effect Log，支持确定性重放
//!
//! Graph 层提供 trait 框架，各业务层（agent/mcp/...）定义自己的 State + Effect。

// ─── Effect ─────────────────────────────────────────────────────

/// 效果 — 描述一次状态转换的领域事件。
///
/// Effect 是不可变的、可序列化的、自包含的。
/// 状态通过 `apply(effect)` 变更，而非直接修改。
pub trait Effect: Sized + serde::Serialize + serde::de::DeserializeOwned {
    /// 将此 Effect 合并到另一个同类型 Effect 中（可选）。
    ///
    /// 用于批量场景：多个 Effect 合并为一个，减少 apply 次数。
    /// 默认返回 `None` 表示不可合并。
    fn combine(self, _other: Self) -> Option<Self> {
        None
    }
}

// ─── WorkflowState ──────────────────────────────────────────────

/// 工作流状态 — 编译期类型安全的状态容器。
///
/// 替代 `HashMap<String, Value>` 动态模型。
/// 每个工作流定义自己的 State struct 和 Effect enum，
/// 实现此 trait 以声明状态转换规则。
///
/// # 示例
///
/// ```rust,ignore
/// pub enum AgentEffect {
///     AppendMessage(Message),
///     IncrementIteration,
///     RecordOutputTokens(usize),
/// }
///
/// pub struct AgentState {
///     pub messages: Vec<Message>,
///     pub iterations: usize,
///     pub output_tokens: usize,
/// }
///
/// impl WorkflowState for AgentState {
///     type Effect = AgentEffect;
///
///     fn apply(&mut self, effect: Self::Effect) {
///         match effect {
///             AgentEffect::AppendMessage(msg) => self.messages.push(msg),
///             AgentEffect::IncrementIteration => self.iterations += 1,
///             AgentEffect::RecordOutputTokens(n) => self.output_tokens += n,
///         }
///     }
///
///     fn merge(mut self, other: Self) -> Result<Self, WorkflowError> {
///         self.messages.extend(other.messages);
///         self.iterations = self.iterations.max(other.iterations);
///         self.output_tokens += other.output_tokens;
///         Ok(self)
///     }
/// }
/// ```
pub trait WorkflowState: Clone + Send + Sync {
    /// 与此状态关联的 Effect 类型。
    type Effect: Effect;

    /// 应用一个 Effect 到状态。
    fn apply(&mut self, effect: Self::Effect);

    /// 批量应用 Effect（默认逐个 apply）。
    fn apply_batch(&mut self, effects: impl IntoIterator<Item = Self::Effect>) {
        for effect in effects {
            self.apply(effect);
        }
    }

    /// 合并两个状态（并行分支合并场景）。
    ///
    /// `self` 是左侧分支，`other` 是右侧分支。
    /// 返回合并后的状态，或合并冲突错误。
    fn merge(self, other: Self) -> Result<Self, WorkflowError>;

    /// 应用一个 BranchState 变更记录到状态（backward compat）。
    ///
    /// 默认实现：no-op（纯 Effect 驱动的状态不需要此方法）。
    /// `State`（HashMap wrapper）覆盖此方法，将 ChangeRecord 转换为 StateEffect。
    fn apply_branch_change(&mut self, _change: &crate::branch_state::ChangeRecord) {
        // no-op — pure effect-driven states don't use BranchState changes
    }

    /// 创建默认/初始状态。
    fn initial() -> Self
    where
        Self: Default,
    {
        Self::default()
    }
}

// ─── WorkflowError ──────────────────────────────────────────────

/// 工作流状态操作错误。
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    /// 状态合并冲突
    #[error("state merge conflict: {0}")]
    MergeConflict(String),

    /// Effect 应用失败
    #[error("failed to apply effect: {0}")]
    ApplyFailed(String),

    /// 状态序列化/反序列化失败
    #[error("state serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
