//! StateKey<T> — 从 lellm-runtime re-export，保持向后兼容。
//!
//! 编译期类型安全的 State 键，绑定 key ↔ type ↔ reducer 的关系。

pub use lellm_runtime::{Reducer, StateKey, StateKeyExt};

// ─── 内置常用 StateKey 常量 ───────────────────────────────────

/// 消息列表 — Graph 中最通用的 State key。
///
/// 用于在节点之间传递对话历史。类型使用 `Vec<serde_json::Value>` 以保持通用性。
///
/// ```rust,ignore
/// use lellm_graph::{StateKeyExt, SK_MESSAGES};
///
/// state.set_sk(&SK_MESSAGES, messages);
/// let msgs = state.get_sk(&SK_MESSAGES).unwrap_or_default();
/// ```
pub static SK_MESSAGES: StateKey<Vec<serde_json::Value>> =
    StateKey::new("messages", Reducer::Append);

/// 通用计数 — 循环计数器等场景。
pub static SK_COUNT: StateKey<u64> = StateKey::new("count", Reducer::Sum);

/// 执行步骤记录 — Barrier 多轮审批等场景。
pub static SK_STEPS: StateKey<Vec<String>> = StateKey::new("steps", Reducer::Append);
