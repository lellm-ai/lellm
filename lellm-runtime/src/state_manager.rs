//! StateManager — 统一 State + Snapshot 管理。
//!
//! **核心原则：** apply 和 record 是原子动作。
//! 消灭双入口，避免遗漏。

use crate::checkpoint::IncrementalSnapshotState;
use crate::delta::{DeltaOp, DeltaSource, Reducer, ReducerRegistry, StateDelta};
use crate::state::{State, StateError};

/// State + Snapshot 统一管理器。
///
/// 所有 State 修改必须通过此结构体，
/// 确保 apply 和 record 是原子动作。
pub struct StateManager {
    /// 当前 State
    pub state: State,
    /// 增量快照状态
    pub snapshot: IncrementalSnapshotState,
}

impl StateManager {
    /// 创建新的 StateManager。
    pub fn new(initial_state: State, compact_threshold: usize) -> Self {
        Self {
            state: initial_state,
            snapshot: IncrementalSnapshotState::new(compact_threshold),
        }
    }

    /// 从 Checkpoint 恢复。
    pub fn from_checkpoint(
        checkpoint_state: State,
        compact_threshold: usize,
    ) -> Self {
        Self {
            state: checkpoint_state,
            snapshot: IncrementalSnapshotState::new(compact_threshold),
        }
    }

    /// 应用单个 Delta 到 State（原子操作：apply + record）。
    pub fn apply_delta(
        &mut self,
        delta: &StateDelta,
    ) -> Result<(), StateError> {
        match delta.op {
            DeltaOp::Put => {
                self.state.insert(delta.key.to_string(), delta.value.clone());
            }
            DeltaOp::Delete => {
                self.state.remove(delta.key.as_ref());
            }
        }
        // 记录到增量快照
        self.snapshot.record_delta(delta.clone());
        Ok(())
    }

    /// 应用多个 Delta 到 State（原子操作：apply + record）。
    ///
    /// 使用 ReducerRegistry 处理多 writer 冲突。
    pub fn apply_deltas(
        &mut self,
        registry: &ReducerRegistry,
        deltas: &[StateDelta],
    ) -> Result<(), StateError> {
        // 使用 ReducerRegistry 合并（与 executor 逻辑一致）
        registry.merge_deltas(&mut self.state, deltas)?;

        // 记录到增量快照
        self.snapshot.record_deltas(deltas.to_vec());

        Ok(())
    }

    /// 应用多个 Delta（无 Reducer，单分支场景）。
    ///
    /// ⚠️ **警告：** 此方法不使用 Reducer，并行场景下可能产生错误结果。
    pub fn apply_deltas_simple(
        &mut self,
        deltas: &[StateDelta],
    ) {
        for delta in deltas {
            match delta.op {
                DeltaOp::Put => {
                    self.state.insert(delta.key.to_string(), delta.value.clone());
                }
                DeltaOp::Delete => {
                    self.state.remove(delta.key.as_ref());
                }
            }
        }
        // 记录到增量快照
        self.snapshot.record_deltas(deltas.to_vec());
    }

    /// 获取当前 State 的引用。
    pub fn state(&self) -> &State {
        &self.state
    }

    /// 获取当前 State 的可变引用。
    pub fn state_mut(&mut self) -> &mut State {
        &mut self.state
    }

    /// 获取增量快照状态的引用。
    pub fn snapshot(&self) -> &IncrementalSnapshotState {
        &self.snapshot
    }

    /// 获取增量快照状态的可变引用。
    pub fn snapshot_mut(&mut self) -> &mut IncrementalSnapshotState {
        &mut self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_delta_records() {
        let mut sm = StateManager::new(State::new(), 20);
        let delta = StateDelta::put("key", serde_json::json!("value"));

        sm.apply_delta(&delta).unwrap();

        assert_eq!(sm.state.get("key"), Some(&serde_json::json!("value")));
        assert_eq!(sm.snapshot.pending_deltas.len(), 1);
    }

    #[test]
    fn test_apply_deltas_records() {
        let mut sm = StateManager::new(State::new(), 20);
        let mut registry = ReducerRegistry::new();
        registry.register("items", Reducer::Append);

        let deltas = vec![
            StateDelta::put("items", serde_json::json!([1, 2])),
            StateDelta::put("items", serde_json::json!([3, 4])),
        ];

        sm.apply_deltas(&registry, &deltas).unwrap();

        let items = sm.state.get("items").unwrap().as_array().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(sm.snapshot.pending_deltas.len(), 2);
    }
}
