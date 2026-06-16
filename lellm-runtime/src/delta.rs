//! StateDelta + Reducer — 键级状态增量与合并策略。
//!
//! **核心设计：**
//! - `DeltaOp` 描述"我想做什么"（修改意图）
//! - `Reducer` 描述"这个 key 允许怎么合并"（合并策略）
//! - Apply 时检查 DeltaOp 是否被该 key 的 Reducer 允许

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::StateError;

/// 状态增量 — 节点对 State 的修改意图。
///
/// 节点输出 Delta，不直接修改 State。Executor 收集所有 Delta 后统一 apply。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDelta {
    /// 要修改的 key
    pub key: String,
    /// 操作类型
    pub op: DeltaOp,
    /// 新值（Delete 操作时忽略）
    pub value: Value,
    /// 产生此 Delta 的节点名称（用于冲突诊断）
    pub writer: Option<String>,
}

impl StateDelta {
    pub fn set(key: impl Into<String>, value: Value) -> Self {
        Self {
            key: key.into(),
            op: DeltaOp::Set,
            value,
            writer: None,
        }
    }

    pub fn delete(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            op: DeltaOp::Delete,
            value: Value::Null,
            writer: None,
        }
    }

    pub fn append(key: impl Into<String>, items: Value) -> Self {
        Self {
            key: key.into(),
            op: DeltaOp::Append,
            value: items,
            writer: None,
        }
    }

    pub fn merge_object(key: impl Into<String>, patch: Value) -> Self {
        Self {
            key: key.into(),
            op: DeltaOp::MergeObject,
            value: patch,
            writer: None,
        }
    }

    pub fn with_writer(mut self, writer: impl Into<String>) -> Self {
        self.writer = Some(writer.into());
        self
    }
}

/// Delta 操作类型 — 描述"我想做什么"（修改意图）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeltaOp {
    /// 覆盖 — 完全替换 key 的值
    Set,
    /// 删除 — 移除 key
    Delete,
    /// 追加 — 追加到数组（目标必须是 Array）
    Append,
    /// 浅合并 — 对象浅合并（目标必须是 Object）
    MergeObject,
    /// 数值相加 — 数值相加（目标必须是 Number）
    Sum,
    /// 取较大值 — 数值取较大值（目标必须是 Number）
    Max,
    /// 取较小值 — 数值取较小值（目标必须是 Number）
    Min,
}

/// Reducer 枚举 — 描述"这个 key 允许怎么合并"（合并策略）。
///
/// 当多个节点（尤其是并行分支）写入同一 key 时，
/// Reducer 决定如何合并冲突。
///
/// **Custom 变体使用函数指针**（`fn`），保证 `Reducer: Copy`，
/// 从而支持 `const StateKey` 定义。如需捕获环境，通过
/// `ReducerRegistry::register_custom()` 在运行时注册。
#[allow(unpredictable_function_pointer_comparisons)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reducer {
    /// 默认 — 冲突即报错（最后写入者胜 = 不确定谁最后）
    Error,
    /// 最后写入者胜 — 覆盖
    Replace,
    /// 数组追加 — 将所有写入追加到数组
    Append,
    /// 对象浅合并 — 合并 object 的顶层字段
    MergeObject,
    /// 数值求和
    Sum,
    /// 取最大值
    Max,
    /// 取最小值
    Min,
    /// 自定义合并函数（函数指针，无捕获环境）
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

/// Reducer 注册表 — 管理每个 key 的合并策略。
///
/// 线程安全，可在节点创建时注册，执行时查询。
///
/// 支持两种注册方式：
/// - `register()` — 使用内置 Reducer 变体（Copy，可 const 定义）
/// - `register_custom()` — 使用运行时闭包（捕获环境）
#[derive(Default)]
pub struct ReducerRegistry {
    reducers: std::collections::HashMap<String, Reducer>,
    /// 运行时注册的自定义闭包 Reducer（优先级高于内置 Reducer）
    custom_reducers: std::collections::HashMap<
        String,
        Box<dyn Fn(&Value, &Value) -> Result<Value, String> + Send + Sync>,
    >,
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

    /// 注册 key 的内置 Reducer。
    pub fn register(&mut self, key: &str, reducer: Reducer) {
        self.reducers.insert(key.to_string(), reducer);
    }

    /// 注册 key 的自定义闭包 Reducer（优先级高于内置 Reducer）。
    pub fn register_custom(
        &mut self,
        key: &str,
        f: impl Fn(&Value, &Value) -> Result<Value, String> + Send + Sync + 'static,
    ) {
        self.custom_reducers.insert(key.to_string(), Box::new(f));
    }

    /// 查询 key 的 Reducer（未注册则返回 Error）。
    ///
    /// 注意：如果 key 有自定义闭包 Reducer，此方法返回 `Custom` 不会被命中——
    /// 调用方应优先调用 `apply_custom()` 检查闭包。
    pub fn get(&self, key: &str) -> &Reducer {
        self.reducers.get(key).unwrap_or(&Reducer::Error)
    }

    /// 应用自定义闭包 Reducer（如果已注册）。
    ///
    /// 返回 `Ok(true)` 表示已应用自定义 Reducer；
    /// 返回 `Ok(false)` 表示无自定义 Reducer，应使用 `get()` 的返回值。
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

    /// 应用 Delta 到 State。
    ///
    /// 根据 Delta 的 op 和 key 注册的 Reducer，执行合并。
    pub fn apply_delta(
        &self,
        state: &mut std::collections::HashMap<String, Value>,
        delta: &StateDelta,
    ) -> Result<(), StateError> {
        // DeltaOp 已描述具体操作，直接 apply
        match delta.op {
            DeltaOp::Set => {
                state.insert(delta.key.clone(), delta.value.clone());
            }
            DeltaOp::Delete => {
                state.remove(&delta.key);
            }
            DeltaOp::Append => {
                let items = delta.value.as_array().ok_or_else(|| {
                    StateError::DeltaApply(delta.key.clone(), "append expects array".into())
                })?;

                if let Some(existing) = state.get(&delta.key) {
                    match existing.as_array() {
                        Some(base) => {
                            let mut merged = base.clone();
                            merged.extend(items.iter().cloned());
                            state.insert(delta.key.clone(), Value::Array(merged));
                        }
                        _ => {
                            return Err(StateError::DeltaApply(
                                delta.key.clone(),
                                "existing value is not an array".into(),
                            ));
                        }
                    }
                } else {
                    state.insert(delta.key.clone(), delta.value.clone());
                }
            }
            DeltaOp::MergeObject => {
                let patch = delta.value.as_object().ok_or_else(|| {
                    StateError::DeltaApply(delta.key.clone(), "merge_object expects object".into())
                })?;

                if let Some(existing) = state.get_mut(&delta.key) {
                    if let Some(obj) = existing.as_object_mut() {
                        for (k, v) in patch {
                            obj.insert(k.clone(), v.clone());
                        }
                    } else {
                        return Err(StateError::DeltaApply(
                            delta.key.clone(),
                            "existing value is not an object".into(),
                        ));
                    }
                } else {
                    state.insert(delta.key.clone(), delta.value.clone());
                }
            }
            DeltaOp::Sum | DeltaOp::Max | DeltaOp::Min => {
                let new_num = delta.value.as_f64().ok_or_else(|| {
                    StateError::DeltaApply(delta.key.clone(), "numeric op expects number".into())
                })?;

                if let Some(existing) = state.get(&delta.key) {
                    let old_num = existing.as_f64().ok_or_else(|| {
                        StateError::DeltaApply(
                            delta.key.clone(),
                            "existing value is not a number".into(),
                        )
                    })?;

                    let result = match delta.op {
                        DeltaOp::Sum => old_num + new_num,
                        DeltaOp::Max => old_num.max(new_num),
                        DeltaOp::Min => old_num.min(new_num),
                        _ => unreachable!(),
                    };
                    state.insert(delta.key.clone(), Value::from(result));
                } else {
                    state.insert(delta.key.clone(), delta.value.clone());
                }
            }
        }

        Ok(())
    }

    /// 合并多个并行分支产生的 Delta。
    ///
    /// 当同一 key 被多个 writer 写入时，根据 Reducer 策略处理冲突。
    pub fn merge_deltas(
        &self,
        state: &mut std::collections::HashMap<String, Value>,
        deltas: &[StateDelta],
    ) -> Result<(), StateError> {
        // 按 key 分组
        let mut grouped: std::collections::HashMap<&str, Vec<&StateDelta>> =
            std::collections::HashMap::new();
        for delta in deltas {
            grouped.entry(&delta.key).or_default().push(delta);
        }

        // 逐 key 处理
        for (key, key_deltas) in grouped {
            let reducer = self.get(key);

            if key_deltas.len() > 1 {
                // 多个 writer 写入同一 key
                match reducer {
                    Reducer::Error => {
                        let writers: Vec<String> =
                            key_deltas.iter().filter_map(|d| d.writer.clone()).collect();
                        return Err(StateError::StateConflict {
                            key: key.to_string(),
                            writers,
                        });
                    }
                    Reducer::Replace => {
                        // 最后写入者胜
                        if let Some(last) = key_deltas.last() {
                            state.insert(key.to_string(), last.value.clone());
                        }
                    }
                    Reducer::Append => {
                        // 收集所有数组项
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
                    }
                    Reducer::Sum | Reducer::Max | Reducer::Min => {
                        let existing_val = state.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);

                        let values: Vec<f64> =
                            key_deltas.iter().filter_map(|d| d.value.as_f64()).collect();

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
                    }
                    Reducer::Custom(f) => {
                        // 依次 apply 自定义 reducer
                        let mut current = state.get(key).cloned().unwrap_or(Value::Null);
                        for d in key_deltas {
                            current = f(&current, &d.value)
                                .map_err(|e| StateError::ReducerConflict(key.to_string(), e))?;
                        }
                        state.insert(key.to_string(), current);
                    }
                }
            } else if let Some(delta) = key_deltas.first() {
                // 单一 writer，直接 apply
                self.apply_delta(state, delta)?;
            }
        }

        Ok(())
    }
}
