//! State 和执行结果。
//!
//! 提供扁平 KV 状态管理，以及显式的 Reducer 合并机制（P1）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Graph 共享状态。
pub type State = HashMap<String, serde_json::Value>;

/// State 操作错误。
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Key 不存在
    #[error("state key '{0}' is missing")]
    MissingKey(String),

    /// 反序列化失败
    #[error("failed to deserialize state key '{0}': {1}")]
    Deserialize(String, String),
}

/// State Reducer 类型别名 — 将已有值与新值合并。
///
/// 类似于 LangGraph 的 `operator.add`，但保持显式：
/// ```rust,ignore
/// // 追加消息列表
/// state.reduce("messages", new_msgs, |existing, new| {
///     let mut msgs: Vec<Value> = serde_json::from_value(existing.clone())?;
///     let additions: Vec<Value> = serde_json::from_value(new.clone())?;
///     msgs.extend(additions);
///     Ok(serde_json::to_value(msgs)?)
/// });
/// ```
pub type StateReducer = Box<
    dyn Fn(&serde_json::Value, &serde_json::Value) -> Result<serde_json::Value, String>
        + Send
        + Sync,
>;

/// State 扩展方法 — 通过 Trait 为 HashMap 添加强类型访问与 Reducer 能力。
pub trait StateExt {
    // ─── 强类型 Getter ────────────────────────────────────────

    /// 获取 String 值。
    fn get_str(&self, key: &str) -> Option<&str>;

    /// 获取 bool 值。
    fn get_bool(&self, key: &str) -> Option<bool>;

    /// 获取 u64 值。
    fn get_u64(&self, key: &str) -> Option<u64>;

    /// 获取 i64 值。
    fn get_i64(&self, key: &str) -> Option<i64>;

    /// 获取 f64 值。
    fn get_f64(&self, key: &str) -> Option<f64>;

    /// 反序列化为强类型。
    fn get_json<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned;

    /// 获取并反序列化为强类型。key 不存在时返回错误。
    fn require<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned;

    /// 设置值（自动序列化）。
    fn set<T>(&mut self, key: impl Into<String>, value: T)
    where
        T: serde::Serialize;

    /// 移除并返回值。
    fn remove(&mut self, key: &str) -> Option<serde_json::Value>;

    /// 检查 key 是否存在。
    fn contains(&self, key: &str) -> bool;

    // ─── Reducer ──────────────────────────────────────────────

    /// 使用 Reducer 合并值到指定 key。
    fn reduce(
        &mut self,
        key: &str,
        value: serde_json::Value,
        reducer: &StateReducer,
    ) -> Result<(), String>;

    /// 追加模式 — 内置的数组追加 Reducer。
    fn append_array(&mut self, key: &str, items: serde_json::Value) -> Result<(), String>;
}

impl StateExt for State {
    fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(|v| v.as_str())
    }

    fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }

    fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.as_u64())
    }

    fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.as_i64())
    }

    fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.as_f64())
    }

    fn get_json<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned,
    {
        let value = self
            .get(key)
            .ok_or_else(|| StateError::MissingKey(key.to_string()))?;
        serde_json::from_value(value.clone())
            .map_err(|e| StateError::Deserialize(key.to_string(), e.to_string()))
    }

    fn require<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned,
    {
        self.get_json(key)
    }

    fn set<T>(&mut self, key: impl Into<String>, value: T)
    where
        T: serde::Serialize,
    {
        let json = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        HashMap::insert(self, key.into(), json);
    }

    fn remove(&mut self, key: &str) -> Option<serde_json::Value> {
        HashMap::remove(self, key)
    }

    fn contains(&self, key: &str) -> bool {
        self.contains_key(key)
    }

    fn reduce(
        &mut self,
        key: &str,
        value: serde_json::Value,
        reducer: &StateReducer,
    ) -> Result<(), String> {
        if let Some(existing) = self.get(key) {
            let merged = reducer(existing, &value)?;
            self.insert(key.to_string(), merged);
        } else {
            self.insert(key.to_string(), value);
        }
        Ok(())
    }

    fn append_array(&mut self, key: &str, items: serde_json::Value) -> Result<(), String> {
        let new_items = items.as_array().ok_or("append_array expects an array")?;
        if let Some(existing) = self.get(key) {
            let mut arr = existing
                .as_array()
                .ok_or("append_array: existing value is not an array")?
                .clone();
            arr.extend(new_items.iter().cloned());
            self.insert(key.to_string(), serde_json::Value::Array(arr));
        } else {
            self.insert(key.to_string(), items);
        }
        Ok(())
    }
}

/// 内置 Reducer：数组追加（类似 LangGraph 的 `operator.add` for lists）。
///
/// ```rust,ignore
/// use lellm_graph::{State, StateExt, array_reducer};
/// let mut state = State::new();
/// state.insert("items", json!([1, 2]));
/// state.reduce("items", json!([3, 4]), &array_reducer())?;
/// // state["items"] == [1, 2, 3, 4]
/// ```
pub fn array_reducer() -> StateReducer {
    Box::new(|existing: &serde_json::Value, new: &serde_json::Value| {
        let mut arr = existing
            .as_array()
            .ok_or("existing value is not an array")?
            .clone();
        let additions = new.as_array().ok_or("new value is not an array")?;
        arr.extend(additions.iter().cloned());
        Ok(serde_json::Value::Array(arr))
    })
}

/// Graph 执行结果。
#[derive(Debug)]
pub struct GraphResult {
    /// 最终状态
    pub state: State,
    /// 执行日志
    pub execution_log: Vec<ExecutionEntry>,
    /// 执行耗时
    pub duration: Duration,
}

/// 单个节点执行记录。
#[derive(Debug, Clone)]
pub struct ExecutionEntry {
    /// 节点名称
    pub node_name: String,
    /// 开始时间
    pub start_time: Instant,
    /// 结束时间
    pub end_time: Instant,
    /// 是否成功
    pub success: bool,
}

impl ExecutionEntry {
    /// 执行耗时
    pub fn elapsed(&self) -> Duration {
        self.end_time.duration_since(self.start_time)
    }
}
