//! StateKey<T> — 编译期类型安全的 State 键。
//!
//! 消除字符串 key 的拼写错误，在编译期绑定 key ↔ type ↔ reducer 的关系。

use serde::{Serialize, de::DeserializeOwned};

use crate::delta::Reducer;
use crate::state::{State, StateError};

/// 编译期类型安全的 State 键。
///
/// 将 key 名称、期望的 Rust 类型、以及合并策略（Reducer）三者绑定。
///
/// # 使用方式
///
/// ```rust,ignore
/// // 1. 定义键常量
/// pub static MESSAGES: StateKey<Vec<Message>> =
///     StateKey::new("messages", Reducer::Append);
/// pub static COUNT: StateKey<u64> = StateKey::new("count", Reducer::Sum);
///
/// // 2. 通过 StateKeyExt 的 *_sk 方法读写
/// state.set_sk(&MESSAGES, messages);
/// let msgs: Vec<Message> = state.require_sk(&MESSAGES)?;
/// ```
#[derive(Debug)]
pub struct StateKey<T> {
    /// State 中存储的 key 名称
    name: &'static str,
    /// 合并策略
    reducer: Reducer,
    /// 类型标记（仅用于编译期类型安全）
    _marker: std::marker::PhantomData<T>,
}

impl<T> StateKey<T> {
    /// 创建类型安全的 State 键常量，绑定合并策略。
    pub const fn new(name: &'static str, reducer: Reducer) -> Self {
        Self {
            name,
            reducer,
            _marker: std::marker::PhantomData,
        }
    }

    /// 获取 key 的字符串名称。
    pub fn name(&self) -> &str {
        self.name
    }

    /// 获取 key 绑定的合并策略。
    pub fn reducer(&self) -> &Reducer {
        &self.reducer
    }
}

// ─── StateKeyExt 扩展：StateKey 专用方法 ─────────────────────────

/// StateKey 专用的 State 扩展方法。
///
/// 通过 trait 为 `State`（`HashMap<String, Value>`）添加类型安全的访问器。
pub trait StateKeyExt {
    /// 使用 StateKey 设置值（自动序列化）。
    fn set_sk<T>(&mut self, key: &StateKey<T>, value: T)
    where
        T: Serialize;

    /// 使用 StateKey 获取值（反序列化为 T）。
    /// Key 不存在时返回 `None`。
    fn get_sk<T>(&self, key: &StateKey<T>) -> Option<T>
    where
        T: DeserializeOwned;

    /// 使用 StateKey 获取并反序列化。
    /// Key 不存在时返回 `StateError::MissingKey`。
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
        let key_str = key.name().to_string();
        let json = match serde_json::to_value(value) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(key = %key_str, error = %e, "failed to serialize state value, storing null");
                serde_json::Value::Null
            }
        };
        self.insert(key_str, json);
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
