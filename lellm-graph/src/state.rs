//! State 和执行结果。
//!
//! 提供扁平 KV 状态管理，以及显式的 Reducer 合并机制（P1）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Graph 共享状态。
pub type State = HashMap<String, serde_json::Value>;

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

/// State 扩展方法 — 通过 Trait 为 HashMap 添加 Reducer 能力。
pub trait StateExt {
    /// 使用 Reducer 合并值到指定 key。
    ///
    /// - 若 key 不存在，直接插入 `value`
    /// - 若 key 已存在，调用 `reducer(existing, &value)` 得到合并后的值
    fn reduce(
        &mut self,
        key: &str,
        value: serde_json::Value,
        reducer: &StateReducer,
    ) -> Result<(), String>;

    /// 追加模式 — 内置的数组追加 Reducer。
    ///
    /// 将 `items`（数组）追加到 key 对应的现有数组末尾。
    /// 若 key 不存在，则直接插入 `items`。
    fn append_array(&mut self, key: &str, items: serde_json::Value) -> Result<(), String>;
}

impl StateExt for State {
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
