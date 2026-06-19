//! StateDelta + Reducer — 键级状态增量与合并策略。
//!
//! 从 lellm-runtime 合并到 lellm-graph。

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::StateError;

/// Delta 来源 — 追踪谁产生了这个修改。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeltaSource {
    /// 节点执行产生
    Node { node_id: String },
    /// Agent Hook 产生
    Hook { node_id: String, hook_name: String },
    /// Reducer 合并产生
    ReducerMerge,
    /// 恢复时重放
    ResumeReplay,
}

impl std::fmt::Display for DeltaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeltaSource::Node { node_id } => write!(f, "node:{}", node_id),
            DeltaSource::Hook { node_id, hook_name } => {
                write!(f, "hook:{}:{}", node_id, hook_name)
            }
            DeltaSource::ReducerMerge => write!(f, "reducer_merge"),
            DeltaSource::ResumeReplay => write!(f, "resume_replay"),
        }
    }
}

/// 状态增量 — 节点对 State 的修改意图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDelta {
    pub key: Cow<'static, str>,
    pub op: DeltaOp,
    pub value: Value,
    pub source: DeltaSource,
}

impl StateDelta {
    pub fn put(key: impl Into<String>, value: Value) -> Self {
        Self {
            key: Cow::Owned(key.into()),
            op: DeltaOp::Put,
            value,
            source: DeltaSource::Node {
                node_id: String::new(),
            },
        }
    }

    pub fn delete(key: impl Into<String>) -> Self {
        Self {
            key: Cow::Owned(key.into()),
            op: DeltaOp::Delete,
            value: Value::Null,
            source: DeltaSource::Node {
                node_id: String::new(),
            },
        }
    }

    pub fn put_with_source(key: impl Into<String>, value: Value, source: DeltaSource) -> Self {
        Self {
            key: Cow::Owned(key.into()),
            op: DeltaOp::Put,
            value,
            source,
        }
    }

    pub fn delete_with_source(key: impl Into<String>, source: DeltaSource) -> Self {
        Self {
            key: Cow::Owned(key.into()),
            op: DeltaOp::Delete,
            value: Value::Null,
            source,
        }
    }

    pub fn with_writer(mut self, writer: impl Into<String>) -> Self {
        self.source = DeltaSource::Node {
            node_id: writer.into(),
        };
        self
    }

    pub fn with_source(mut self, source: DeltaSource) -> Self {
        self.source = source;
        self
    }
}

/// Delta 操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaOp {
    /// 覆盖写入
    Put,
    /// 删除
    Delete,
}

/// Reducer 枚举 — 描述"这个 key 允许怎么合并"。
#[allow(unpredictable_function_pointer_comparisons)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reducer {
    /// 冲突即报错
    Error,
    /// 最后写入者胜
    Replace,
    /// 数组追加
    Append,
    /// 对象浅合并
    MergeObject,
    /// 数值求和
    Sum,
    /// 取最大值
    Max,
    /// 取最小值
    Min,
    /// 自定义合并函数
    Custom(fn(&Value, &Value) -> Result<Value, String>),
}

impl std::fmt::Display for Reducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reducer::Error => write!(f, "Error"),
            Reducer::Replace => write!(f, "Replace"),
            Reducer::Append => write!(f, "Append"),
            Reducer::MergeObject => write!(f, "MergeObject"),
            Reducer::Sum => write!(f, "Sum"),
            Reducer::Max => write!(f, "Max"),
            Reducer::Min => write!(f, "Min"),
            Reducer::Custom(_) => write!(f, "Custom"),
        }
    }
}

/// 自定义 Reducer 闭包类型。
type CustomReducerFn = Box<dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync>;

/// Reducer 注册表 — 管理每个 key 的合并策略。
#[derive(Default)]
pub struct ReducerRegistry {
    reducers: std::collections::HashMap<String, Reducer>,
    custom_reducers: std::collections::HashMap<String, CustomReducerFn>,
}

impl std::fmt::Debug for ReducerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReducerRegistry")
            .field("reducers", &self.reducers)
            .field(
                "custom_reducers",
                &format!("{} entries", self.custom_reducers.len()),
            )
            .finish()
    }
}

impl ReducerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, key: &str, reducer: Reducer) {
        self.reducers.insert(key.to_string(), reducer);
    }

    pub fn register_custom(
        &mut self,
        key: &str,
        f: impl Fn(&Value, &Value) -> Result<Value, String> + Send + Sync + 'static,
    ) {
        self.custom_reducers.insert(key.to_string(), Box::new(f));
    }

    pub fn get(&self, key: &str) -> &Reducer {
        self.reducers.get(key).unwrap_or(&Reducer::Error)
    }

    pub fn apply_custom(
        &self,
        key: &str,
        existing: &Value,
        new_val: &Value,
    ) -> Result<Option<Value>, String> {
        if let Some(f) = self.custom_reducers.get(key) {
            Ok(Some(f(existing, new_val)?))
        } else {
            Ok(None)
        }
    }

    pub fn apply_delta(
        &self,
        state: &mut std::collections::HashMap<String, Value>,
        delta: &StateDelta,
    ) -> Result<(), StateError> {
        match delta.op {
            DeltaOp::Put => {
                state.insert(delta.key.to_string(), delta.value.clone());
            }
            DeltaOp::Delete => {
                state.remove(delta.key.as_ref());
            }
        }
        Ok(())
    }

    pub fn merge_deltas(
        &self,
        state: &mut std::collections::HashMap<String, Value>,
        deltas: &[StateDelta],
    ) -> Result<(), StateError> {
        let mut grouped: std::collections::HashMap<&str, Vec<&StateDelta>> =
            std::collections::HashMap::new();
        for delta in deltas {
            grouped.entry(&delta.key).or_default().push(delta);
        }

        for (key, key_deltas) in grouped {
            if key_deltas.len() > 1 {
                self.merge_by_reducer(state, key, &key_deltas, self.get(key))?;
            } else if let Some(delta) = key_deltas.first() {
                self.apply_delta(state, delta)?;
            }
        }

        Ok(())
    }

    fn merge_by_reducer(
        &self,
        state: &mut std::collections::HashMap<String, Value>,
        key: &str,
        key_deltas: &[&StateDelta],
        reducer: &Reducer,
    ) -> Result<(), StateError> {
        match reducer {
            Reducer::Error => {
                let writers: Vec<String> =
                    key_deltas.iter().map(|d| d.source.to_string()).collect();
                Err(StateError::StateConflict {
                    key: key.to_string(),
                    writers,
                })
            }
            Reducer::Replace => {
                if let Some(last) = key_deltas.last() {
                    state.insert(key.to_string(), last.value.clone());
                }
                Ok(())
            }
            Reducer::Append => {
                let mut all_items = Vec::new();
                for d in key_deltas {
                    if let Some(arr) = d.value.as_array() {
                        all_items.extend(arr.iter().cloned());
                    }
                }
                if let Some(existing) = state.get(key).and_then(|v| v.as_array()) {
                    let mut merged = existing.clone();
                    merged.extend(all_items);
                    state.insert(key.to_string(), Value::Array(merged));
                } else if !all_items.is_empty() {
                    state.insert(key.to_string(), Value::Array(all_items));
                }
                Ok(())
            }
            Reducer::MergeObject => {
                let mut merged = state
                    .get(key)
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                for d in key_deltas {
                    if let Some(obj) = d.value.as_object() {
                        for (k, v) in obj {
                            merged.insert(k.clone(), v.clone());
                        }
                    }
                }
                state.insert(key.to_string(), Value::Object(merged));
                Ok(())
            }
            Reducer::Sum | Reducer::Max | Reducer::Min => {
                let existing_val = state.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let values: Vec<f64> = key_deltas.iter().filter_map(|d| d.value.as_f64()).collect();

                let result = if values.is_empty() {
                    existing_val
                } else {
                    let sum: f64 = values.iter().sum();
                    match reducer {
                        Reducer::Sum => existing_val + sum,
                        Reducer::Max => existing_val.max(
                            *values
                                .iter()
                                .max_by(|a, b| a.partial_cmp(b).unwrap())
                                .unwrap(),
                        ),
                        Reducer::Min => existing_val.min(
                            *values
                                .iter()
                                .min_by(|a, b| a.partial_cmp(b).unwrap())
                                .unwrap(),
                        ),
                        _ => unreachable!(),
                    }
                };
                state.insert(key.to_string(), Value::from(result));
                Ok(())
            }
            Reducer::Custom(f) => {
                let mut current = state.get(key).cloned().unwrap_or(Value::Null);
                for d in key_deltas {
                    current = f(&current, &d.value)
                        .map_err(|e| StateError::ReducerConflict(key.to_string(), e))?;
                }
                state.insert(key.to_string(), current);
                Ok(())
            }
        }
    }
}
