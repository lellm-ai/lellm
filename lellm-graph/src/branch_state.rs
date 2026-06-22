//! BranchState — Overlay State 模型。
//!
//! 拆成两个类型：
//! - `StateSnapshot` — 不可变的状态快照，对应全量 Checkpoint
//! - `BranchState` — 可写的分支状态，一层 Overlay，对应增量 Checkpoint
//!
//! Overlay 模型的核心约束：永远只有一层 overlay，不是 MVCC 链。

use std::collections::HashMap;
use std::sync::Arc;

use crate::state::State;

// ─── ChangeRecord ─────────────────────────────────────────────

/// 变更操作类型。
#[derive(Debug, Clone)]
pub enum ChangeOperation {
    Put,
    Delete,
}

/// 变更记录 — 忠实记录每次操作，便于审计。
#[derive(Debug, Clone)]
pub struct ChangeRecord {
    pub key: String,
    pub operation: ChangeOperation,
    pub value: serde_json::Value,
}

// ─── BranchState ──────────────────────────────────────────────

/// 可写的分支状态 — 一层 Overlay。
///
/// Overlay 模型的核心约束：永远只有一层 overlay，不是 MVCC 链。
/// - fork = O(1)：深拷贝 base snapshot
/// - 读取 = O(1)：最多查两层（local + base）
/// - 写入 = 自动记 ChangeRecord
pub struct BranchState {
    /// 不可变的基态快照
    base: Arc<State>,
    /// 本层写入缓存
    local: HashMap<String, serde_json::Value>,
    /// 变更日志
    changes: Vec<ChangeRecord>,
}

impl Clone for BranchState {
    fn clone(&self) -> Self {
        Self {
            base: Arc::clone(&self.base),
            local: self.local.clone(),
            changes: self.changes.clone(),
        }
    }
}

impl std::fmt::Debug for BranchState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchState")
            .field("base_keys", &self.base.len())
            .field("local_keys", &self.local.len())
            .field("changes", &self.changes.len())
            .finish()
    }
}

impl BranchState {
    /// 从 State 创建 BranchState（基态）。
    pub fn from_state(state: State) -> Self {
        Self {
            base: Arc::new(state),
            local: HashMap::new(),
            changes: Vec::new(),
        }
    }

    /// 创建新的基态 BranchState（空状态）。
    pub fn empty() -> Self {
        Self::from_state(State::new())
    }

    // ─── 读取 — O(1) ──────────────────────────────────────

    /// 读取值。最多查两层（local + base）。
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.local.get(key).or_else(|| self.base.get(key))
    }

    /// 获取原始 Value 引用（用于路由条件判断）。
    pub fn get_ref(&self, key: &str) -> Option<&serde_json::Value> {
        self.local.get(key).or_else(|| self.base.get(key))
    }

    /// 读取并反序列化。
    pub fn get_typed<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    // ─── 写入 — 自动记 ChangeRecord ────────────────────────

    /// 写入值。
    pub fn set(&mut self, key: impl Into<String>, value: serde_json::Value) {
        let key = key.into();
        self.changes.push(ChangeRecord {
            key: key.clone(),
            operation: ChangeOperation::Put,
            value: value.clone(),
        });
        self.local.insert(key, value);
    }

    /// 删除值。
    pub fn remove(&mut self, key: &str) {
        self.changes.push(ChangeRecord {
            key: key.to_string(),
            operation: ChangeOperation::Delete,
            value: serde_json::Value::Null,
        });
        self.local.remove(key);
    }

    // ─── Fork — O(1) ──────────────────────────────────────

    /// Fork 当前状态为新的 BranchState。
    ///
    /// 将当前物化状态作为新的 base，清空 overlay 和变更日志。
    pub fn fork(&self) -> BranchState {
        let state = self.to_state();
        BranchState {
            base: Arc::new(state),
            local: HashMap::new(),
            changes: Vec::new(),
        }
    }

    // ─── 快照导出 ─────────────────────────────────────────

    /// 物化当前状态（base + local changes），用于 Checkpoint。
    pub fn to_state(&self) -> State {
        let mut state = self.base.as_ref().clone();
        for (key, value) in &self.local {
            state.insert(key.clone(), value.clone());
        }
        state
    }

    /// 获取变更日志。
    pub fn changes(&self) -> &[ChangeRecord] {
        &self.changes
    }

    /// 清空变更日志。
    pub fn clear_changes(&mut self) {
        self.changes.clear();
    }

    /// 获取当前所有 key（base + local）。
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.base.keys().cloned().collect();
        for key in self.local.keys() {
            if !keys.contains(key) {
                keys.push(key.clone());
            }
        }
        keys
    }

    /// 检查 key 是否存在。
    pub fn contains(&self, key: &str) -> bool {
        self.local.contains_key(key) || self.base.contains_key(key)
    }

    /// 获取 base 引用。
    pub fn base(&self) -> &State {
        &self.base
    }
}
