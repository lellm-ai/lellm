//! State 和执行结果。
//!
//! 包含 Graph 共享状态的核心类型（从 lellm-runtime 合并）和 Graph 特有的执行结果类型。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

// ─── State 类型 ─────────────────────────────────────────────────

/// Graph 共享状态。
pub type State = HashMap<String, Value>;

// ─── StateError ─────────────────────────────────────────────────

/// State 操作错误。
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// Key 不存在
    #[error("state key '{0}' is missing")]
    MissingKey(String),

    /// 反序列化失败
    #[error("failed to deserialize state key '{0}': {1}")]
    Deserialize(String, String),

    /// Reducer 合并失败
    #[error("reducer conflict on key '{0}': {1}")]
    ReducerConflict(String, String),

    /// Delta 应用失败（类型不匹配等）
    #[error("failed to apply delta on key '{0}': {1}")]
    DeltaApply(String, String),

    /// 并行状态冲突
    #[error("state conflict on key '{key}': concurrent writers [{}]", writers.join(", "))]
    StateConflict { key: String, writers: Vec<String> },
}

// ─── StateExt ───────────────────────────────────────────────────

/// State 扩展方法 trait。
///
/// 为 `State`（`HashMap<String, Value>`）提供类型安全的读写方法。
pub trait StateExt {
    fn get_str(&self, key: &str) -> Option<&str>;
    fn get_bool(&self, key: &str) -> Option<bool>;
    fn get_u64(&self, key: &str) -> Option<u64>;
    fn get_i64(&self, key: &str) -> Option<i64>;
    fn get_f64(&self, key: &str) -> Option<f64>;

    fn get_json<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned;

    fn require<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned;

    fn set<T>(&mut self, key: impl Into<String>, value: T)
    where
        T: serde::Serialize;

    fn remove(&mut self, key: &str) -> Option<serde_json::Value>;
    fn contains(&self, key: &str) -> bool;

    fn reduce(
        &mut self,
        key: &str,
        value: serde_json::Value,
        reducer: &StateReducer,
    ) -> Result<(), String>;

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
        let key_str = key.into();
        let json = match serde_json::to_value(value) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(key = %key_str, error = %e, "failed to serialize state value, storing null");
                serde_json::Value::Null
            }
        };
        HashMap::insert(self, key_str, json);
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

/// State Reducer 类型别名 — 将已有值与新值合并。
pub type StateReducer = Box<dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync>;

/// 内置 Reducer：数组追加。
pub fn array_reducer(existing: &Value, new: &Value) -> Result<Value, String> {
    let base = existing
        .as_array()
        .ok_or("array_reducer: existing is not an array")?;
    let items = new
        .as_array()
        .ok_or("array_reducer: new value is not an array")?;
    let mut merged = base.clone();
    merged.extend(items.iter().cloned());
    Ok(Value::Array(merged))
}

// ─── GraphResult ────────────────────────────────────────────────

/// Graph 执行结果。
#[derive(Debug)]
pub struct GraphResult {
    /// 执行追踪 ID（关联本次执行的所有 SpanId）
    pub trace_id: crate::ids::TraceId,
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
    /// 全局步数（第几步）
    pub step: usize,
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
