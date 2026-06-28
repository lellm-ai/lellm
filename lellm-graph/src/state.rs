//! State 和执行结果。
//!
//! 包含 Graph 共享状态的核心类型（从 lellm-runtime 合并）和 Graph 特有的执行结果类型。
//!
//! v0.4+: `State` 从 type alias 改为 struct wrapper，以便实现 `WorkflowState` trait。
//! 通过 `Deref`/`DerefMut` 保持对 `HashMap<String, Value>` 的完全兼容。

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

// ─── State 类型 ─────────────────────────────────────────────────

/// Graph 共享状态 — struct wrapper，支持 `WorkflowState` trait。
///
/// 通过 `Deref`/`DerefMut` 完全兼容 `HashMap<String, Value>` API。
/// 所有现有代码无需修改。
#[derive(Debug, Clone, Default)]
pub struct State {
    inner: HashMap<String, Value>,
}

/// 手动实现 Serialize/Deserialize — 序列化底层 HashMap，保持兼容。
impl serde::Serialize for State {
    fn serialize<SER: serde::Serializer>(&self, serializer: SER) -> Result<SER::Ok, SER::Error> {
        self.inner.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for State {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = HashMap::deserialize(deserializer)?;
        Ok(State { inner: map })
    }
}

impl State {
    /// 创建空状态。
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }
}

impl Deref for State {
    type Target = HashMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for State {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<HashMap<String, Value>> for State {
    fn from(map: HashMap<String, Value>) -> Self {
        Self { inner: map }
    }
}

impl From<State> for HashMap<String, Value> {
    fn from(state: State) -> Self {
        state.inner
    }
}

// ─── WorkflowState for State ────────────────────────────────────

/// State 的 Mutation — HashMap 级别的变更。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum StateMutation {
    /// 设置 key-value
    Put(String, Value),
    /// 删除 key
    Delete(String),
}

impl crate::workflow_state::StateMutation<State> for StateMutation {
    fn apply(self, state: &mut State) {
        match self {
            StateMutation::Put(key, value) => {
                state.insert(key, value);
            }
            StateMutation::Delete(key) => {
                state.remove(&key);
            }
        }
    }
}

impl crate::workflow_state::WorkflowState for State {
    type Mutation = StateMutation;

    fn apply_branch_change(&mut self, change: &crate::branch_state::ChangeRecord) {
        match change.operation {
            crate::branch_state::ChangeOperation::Put => {
                self.inner.insert(change.key.clone(), change.value.clone());
            }
            crate::branch_state::ChangeOperation::Delete => {
                self.inner.remove(&change.key);
            }
        }
    }
}

/// State 的默认合并策略 — 逐 key 合并，后续分支覆盖同 key。
pub struct StateMerge;

impl crate::workflow_state::MergeStrategy<State> for StateMerge {
    fn merge(branches: Vec<State>) -> Result<State, crate::workflow_state::WorkflowError> {
        let mut merged: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for state in branches {
            merged.extend(state.inner);
        }
        Ok(State {
            inner: merged.into_iter().collect(),
        })
    }

    fn default_instance() -> Self {
        StateMerge
    }
}

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
/// 为 `State` 提供类型安全的读写方法。
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
        self.inner.get(key).and_then(|v| v.as_str())
    }

    fn get_bool(&self, key: &str) -> Option<bool> {
        self.inner.get(key).and_then(|v| v.as_bool())
    }

    fn get_u64(&self, key: &str) -> Option<u64> {
        self.inner.get(key).and_then(|v| v.as_u64())
    }

    fn get_i64(&self, key: &str) -> Option<i64> {
        self.inner.get(key).and_then(|v| v.as_i64())
    }

    fn get_f64(&self, key: &str) -> Option<f64> {
        self.inner.get(key).and_then(|v| v.as_f64())
    }

    fn get_json<T>(&self, key: &str) -> Result<T, StateError>
    where
        T: serde::de::DeserializeOwned,
    {
        let value = self
            .inner
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
        self.inner.insert(key_str, json);
    }

    fn remove(&mut self, key: &str) -> Option<serde_json::Value> {
        self.inner.remove(key)
    }

    fn contains(&self, key: &str) -> bool {
        self.inner.contains_key(key)
    }

    fn reduce(
        &mut self,
        key: &str,
        value: serde_json::Value,
        reducer: &StateReducer,
    ) -> Result<(), String> {
        if let Some(existing) = self.inner.get(key) {
            let merged = reducer(existing, &value)?;
            self.inner.insert(key.to_string(), merged);
        } else {
            self.inner.insert(key.to_string(), value);
        }
        Ok(())
    }

    fn append_array(&mut self, key: &str, items: serde_json::Value) -> Result<(), String> {
        let new_items = items.as_array().ok_or("append_array expects an array")?;
        if let Some(existing) = self.inner.get(key) {
            let mut arr = existing
                .as_array()
                .ok_or("append_array: existing value is not an array")?
                .clone();
            arr.extend(new_items.iter().cloned());
            self.inner
                .insert(key.to_string(), serde_json::Value::Array(arr));
        } else {
            self.inner.insert(key.to_string(), items);
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
///
/// 运行时使用 `Instant` 精确计时，序列化时转换为 ISO-8601 字符串。
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
    /// 错误信息（失败时）
    pub error: Option<String>,
}

impl ExecutionEntry {
    /// 执行耗时
    pub fn elapsed(&self) -> Duration {
        self.end_time.duration_since(self.start_time)
    }

    /// 序列化为 JSON Value（Instant → ISO-8601 字符串）。
    /// 供 Checkpoint 持久化使用。
    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "step": self.step,
            "node_name": self.node_name,
            "start_time": instant_to_iso(&self.start_time),
            "end_time": instant_to_iso(&self.end_time),
            "success": self.success,
            "error": self.error,
        })
    }
}

/// 将 Instant 转换为 ISO-8601 时间戳字符串。
/// 使用 UNIX_EPOCH 近似计算，不依赖 chrono。
fn instant_to_iso(instant: &Instant) -> String {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed_secs = instant.elapsed().as_secs();
    let secs = now_secs.saturating_sub(elapsed_secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        ((secs / 86400 / 365) + 1970) as u16,
        ((secs / 86400 % 365) / 30 + 1) as u8,
        (secs / 86400 % 30 + 1) as u8,
        (secs % 86400 / 3600) as u8,
        (secs % 3600 / 60) as u8,
        (secs % 60) as u8
    )
}
