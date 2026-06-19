//! StateKey<T> — 编译期类型安全的 State 键。
//!
//! 从 lellm-runtime 合并到 lellm-graph，加上内置的常用 StateKey 常量。

use serde::{Serialize, de::DeserializeOwned};

use crate::delta::Reducer;
use crate::state::{State, StateError};

/// 编译期类型安全的 State 键。
#[derive(Debug)]
pub struct StateKey<T> {
    name: &'static str,
    reducer: Reducer,
    _marker: std::marker::PhantomData<T>,
}

impl<T> StateKey<T> {
    pub const fn new(name: &'static str, reducer: Reducer) -> Self {
        Self {
            name,
            reducer,
            _marker: std::marker::PhantomData,
        }
    }

    pub const fn append(name: &'static str) -> Self {
        Self::new(name, Reducer::Append)
    }

    pub const fn sum(name: &'static str) -> Self {
        Self::new(name, Reducer::Sum)
    }

    pub const fn replace(name: &'static str) -> Self {
        Self::new(name, Reducer::Replace)
    }

    pub const fn merge_object(name: &'static str) -> Self {
        Self::new(name, Reducer::MergeObject)
    }

    pub const fn max(name: &'static str) -> Self {
        Self::new(name, Reducer::Max)
    }

    pub const fn min(name: &'static str) -> Self {
        Self::new(name, Reducer::Min)
    }

    pub const fn error(name: &'static str) -> Self {
        Self::new(name, Reducer::Error)
    }

    pub fn name(&self) -> &str {
        self.name
    }

    pub fn reducer(&self) -> &Reducer {
        &self.reducer
    }
}

/// StateKey 专用的 State 扩展方法。
pub trait StateKeyExt {
    fn set_sk<T>(&mut self, key: &StateKey<T>, value: T)
    where
        T: Serialize;

    fn get_sk<T>(&self, key: &StateKey<T>) -> Option<T>
    where
        T: DeserializeOwned;

    fn require_sk<T>(&self, key: &StateKey<T>) -> Result<T, StateError>
    where
        T: DeserializeOwned;

    fn contains_sk<T>(&self, key: &StateKey<T>) -> bool;

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

// ─── 内置常用 StateKey 常量 ───────────────────────────────────

/// 消息列表 — Graph 中最通用的 State key。
pub static SK_MESSAGES: StateKey<Vec<serde_json::Value>> =
    StateKey::new("messages", Reducer::Append);

/// 通用计数 — 循环计数器等场景。
pub static SK_COUNT: StateKey<u64> = StateKey::new("count", Reducer::Sum);

/// 执行步骤记录 — Barrier 多轮审批等场景。
pub static SK_STEPS: StateKey<Vec<String>> = StateKey::new("steps", Reducer::Append);

// ─── Agent 核心状态键（v0.3.1）─────────────────────────────────

/// Agent 迭代轮次。
pub static SK_ITERATIONS: StateKey<u32> = StateKey::replace("iterations");

/// 当前轮待执行的工具调用（每轮清空，非历史累计）。
pub static SK_PENDING_TOOL_CALLS: StateKey<Vec<serde_json::Value>> =
    StateKey::replace("pending_tool_calls");

/// 累计输出 Token 数（Text，不含 Thinking）。
pub static SK_OUTPUT_TOKENS: StateKey<usize> = StateKey::sum("output_tokens");

/// 累计推理 Token 数（Thinking，不含 Text）。
pub static SK_REASONING_TOKENS: StateKey<usize> = StateKey::sum("reasoning_tokens");
