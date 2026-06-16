//! StateKey<T> — 编译期类型安全的 State 键。
//!
//! 消除字符串 key 的拼写错误，在编译期绑定 key ↔ type 的关系。
//!
//! ```rust,ignore
//! // 1. 定义键常量
//! pub const SK_MESSAGES: StateKey<Vec<Message>> = StateKey::new("messages");
//! pub const SK_COUNT: StateKey<u64> = StateKey::new("count");
//!
//! // 2. 通过 StateExt 的 *_sk 方法读写
//! use lellm_graph::StateExt;
//!
//! state.set_sk(&SK_MESSAGES, messages);
//! let msgs: Vec<Message> = state.require_sk(&SK_MESSAGES)?;
//! let count = state.get_sk(&SK_COUNT).unwrap_or(0);
//!
//! // 3. 与现有 API 完全共存
//! state.set("legacy_key", 42);  // 仍然可用
//! ```

use serde::{Serialize, de::DeserializeOwned};

use crate::state::{State, StateError};

/// 编译期类型安全的 State 键。
///
/// 将 key 名称与期望的 Rust 类型绑定，消除拼写错误和类型不匹配。
///
/// # 使用方式
///
/// 1. 定义常量（通常在模块顶层或共享常量文件中）：
///    ```rust,ignore
///    pub const SK_MESSAGES: StateKey<Vec<Message>> = StateKey::new("messages");
///    pub const SK_COUNT: StateKey<u64> = StateKey::new("count");
///    ```
///
/// 2. 通过 `StateExt` 的 `*_sk` 方法读写：
///    ```rust,ignore
///    use lellm_graph::StateExt;
///    state.set_sk(&SK_MESSAGES, messages);
///    let msgs: Vec<Message> = state.require_sk(&SK_MESSAGES)?;
///    ```
#[derive(Debug, Clone, Copy)]
pub struct StateKey<T> {
    /// State 中存储的 key 名称
    name: &'static str,
    /// 类型标记（仅用于编译期类型安全）
    _marker: std::marker::PhantomData<T>,
}

impl<T> StateKey<T> {
    /// 创建类型安全的 State 键常量。
    ///
    /// # 示例
    /// ```rust,ignore
    /// pub const SK_MESSAGES: StateKey<Vec<Message>> = StateKey::new("messages");
    /// pub const SK_COUNT: StateKey<u64> = StateKey::new("count");
    /// ```
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _marker: std::marker::PhantomData,
        }
    }

    /// 获取 key 的字符串名称。
    pub fn name(&self) -> &str {
        self.name
    }
}

// ─── StateExt 扩展：StateKey 专用方法 ─────────────────────────

/// StateKey 专用的 State 扩展方法。
///
/// 通过 trait 为 `State`（`HashMap<String, Value>`）添加类型安全的访问器。
/// 与现有 `StateExt` 方法共存，不破坏向后兼容。
pub trait StateKeyExt {
    // ─── 写入 ──────────────────────────────────────────────────

    /// 使用 StateKey 设置值（自动序列化）。
    ///
    /// 与 `set()` 相比，编译期检查类型。
    fn set_sk<T>(&mut self, key: &StateKey<T>, value: T)
    where
        T: Serialize;

    // ─── 读取 ──────────────────────────────────────────────────

    /// 使用 StateKey 获取值（反序列化为 T）。
    ///
    /// Key 不存在时返回 `None`。
    fn get_sk<T>(&self, key: &StateKey<T>) -> Option<T>
    where
        T: DeserializeOwned;

    /// 使用 StateKey 获取并反序列化。
    ///
    /// Key 不存在时返回 `StateError::MissingKey`。
    /// 反序列化失败时返回 `StateError::Deserialize`。
    fn require_sk<T>(&self, key: &StateKey<T>) -> Result<T, StateError>
    where
        T: DeserializeOwned;

    /// 使用 StateKey 检查 key 是否存在。
    fn contains_sk<T>(&self, key: &StateKey<T>) -> bool;

    /// 使用 StateKey 移除并返回值。
    fn remove_sk<T>(&mut self, key: &StateKey<T>) -> Option<serde_json::Value>;
}

impl StateKeyExt for State {
    fn set_sk<T>(&mut self, key: &StateKey<T>, value: T)
    where
        T: Serialize,
    {
        let json = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        self.insert(key.name().to_string(), json);
    }

    fn get_sk<T>(&self, key: &StateKey<T>) -> Option<T>
    where
        T: DeserializeOwned,
    {
        self.get(key.name())
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    fn require_sk<T>(&self, key: &StateKey<T>) -> Result<T, StateError>
    where
        T: DeserializeOwned,
    {
        let value = self
            .get(key.name())
            .ok_or_else(|| StateError::MissingKey(key.name().to_string()))?;
        serde_json::from_value(value.clone())
            .map_err(|e| StateError::Deserialize(key.name().to_string(), e.to_string()))
    }

    fn contains_sk<T>(&self, key: &StateKey<T>) -> bool {
        self.contains_key(key.name())
    }

    fn remove_sk<T>(&mut self, key: &StateKey<T>) -> Option<serde_json::Value> {
        self.remove(key.name())
    }
}

// ─── 内置常用 StateKey 常量 ───────────────────────────────────

/// 消息列表 — Graph 中最通用的 State key。
///
/// 用于在节点之间传递对话历史（`Vec<lellm_core::Message>`）。
///
/// ```rust,ignore
/// use lellm_graph::{StateKeyExt, SK_MESSAGES};
///
/// state.set_sk(&SK_MESSAGES, messages);
/// let msgs = state.get_sk(&SK_MESSAGES).unwrap_or_default();
/// ```
pub const SK_MESSAGES: StateKey<Vec<lellm_core::Message>> = StateKey::new("messages");

/// 通用计数 — 循环计数器等场景。
pub const SK_COUNT: StateKey<u64> = StateKey::new("count");

/// 执行步骤记录 — Barrier 多轮审批等场景。
pub const SK_STEPS: StateKey<Vec<String>> = StateKey::new("steps");
