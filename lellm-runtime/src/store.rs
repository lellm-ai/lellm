//! Checkpoint 存储后端实现。
//!
//! 提供内存后端（`InMemoryCheckpointStore`），用于开发与测试。

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointId, CheckpointStore, CheckpointStoreError, TraceId};

/// 基于内存的 Checkpoint 存储后端。
///
/// - 线程安全（`RwLock`）
/// - 无自动清理 — 依赖调用方调用 `prune()`
/// - 适合开发与测试，不适合生产（进程重启即丢失）
#[derive(Default)]
pub struct InMemoryCheckpointStore {
    /// checkpoint_id → Checkpoint
    store: RwLock<HashMap<CheckpointId, Checkpoint>>,
    /// trace_id → [CheckpointId] 索引（按时间正序）
    index: RwLock<HashMap<TraceId, Vec<CheckpointId>>>,
}

impl InMemoryCheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 返回存储的 Checkpoint 总数。
    pub fn len(&self) -> usize {
        self.store.read().unwrap().len()
    }

    /// 是否存储为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointStoreError> {
        let ck = checkpoint.clone();
        let id = ck.checkpoint_id.clone();
        let trace = ck.parent_trace_id;

        // 写入存储
        {
            let mut store = self
                .store
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            store.insert(id.clone(), ck);
        }

        // 更新索引
        {
            let mut index = self
                .index
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            index.entry(trace).or_default().push(id);
        }

        Ok(())
    }

    async fn load(&self, id: &CheckpointId) -> Result<Option<Checkpoint>, CheckpointStoreError> {
        let store = self
            .store
            .read()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        Ok(store.get(id).cloned())
    }

    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<Checkpoint>, CheckpointStoreError> {
        // 从索引中获取最后一个 ID
        let last_id = {
            let index = self
                .index
                .read()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            index.get(trace_id).and_then(|ids| ids.last()).cloned()
        };

        match last_id {
            Some(id) => self.load(&id).await,
            None => Ok(None),
        }
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError> {
        let index = self
            .index
            .read()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        // 返回倒序（最新的在前）
        let ids = index.get(trace_id).cloned().unwrap_or_default();
        Ok(ids.into_iter().rev().collect())
    }

    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
        // 先从 store 中删除，获取 trace_id
        let trace_id = {
            let mut store = self
                .store
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            store.remove(id).map(|ck| ck.parent_trace_id)
        };

        match trace_id {
            Some(trace) => {
                // 从索引中移除
                let index = self
                    .index
                    .write()
                    .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
                let mut index = index;
                if let Some(ids) = index.get_mut(&trace) {
                    ids.retain(|iid| iid != id);
                    if ids.is_empty() {
                        index.remove(&trace);
                    }
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError> {
        // 获取要删除的 IDs
        let to_delete: Vec<CheckpointId> = {
            let mut index = self
                .index
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            match index.get_mut(trace_id) {
                Some(ids) if ids.len() > keep => {
                    let remove_count = ids.len() - keep;
                    ids.drain(..remove_count).collect()
                }
                _ => return Ok(0),
            }
        };

        // 从 store 中删除
        let mut store = self
            .store
            .write()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        for id in &to_delete {
            store.remove(id);
        }

        Ok(to_delete.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{Checkpoint, NodeId, TraceId};
    use crate::state::{State, StateExt};

    fn make_checkpoint(trace: TraceId, node: &str, step: u64) -> Checkpoint {
        let mut state = State::new();
        state.set("step", step);
        Checkpoint::new(trace, "test-hash", node, state)
    }

    #[tokio::test]
    async fn test_save_and_load() {
        let store = InMemoryCheckpointStore::new();
        let trace = TraceId::new();
        let ck = make_checkpoint(trace, "node-a", 1);
        let id = ck.checkpoint_id.clone();

        store.save(&ck).await.unwrap();
        assert_eq!(store.len(), 1);

        let loaded = store.load(&id).await.unwrap().expect("should exist");
        assert_eq!(loaded.checkpoint_id, id);
        assert_eq!(loaded.current_node, NodeId("node-a".into()));
    }

    #[tokio::test]
    async fn test_load_latest() {
        let store = InMemoryCheckpointStore::new();
        let trace = TraceId::new();

        store
            .save(&make_checkpoint(trace, "node-a", 1))
            .await
            .unwrap();
        store
            .save(&make_checkpoint(trace, "node-b", 2))
            .await
            .unwrap();
        store
            .save(&make_checkpoint(trace, "node-c", 3))
            .await
            .unwrap();

        assert_eq!(store.len(), 3);

        let latest = store
            .load_latest(&trace)
            .await
            .unwrap()
            .expect("should exist");
        assert_eq!(latest.current_node, NodeId("node-c".into()));
        assert_eq!(latest.get_state_value("step"), Some(3));
    }

    #[tokio::test]
    async fn test_list() {
        let store = InMemoryCheckpointStore::new();
        let trace = TraceId::new();

        let ck1 = make_checkpoint(trace, "node-a", 1);
        let ck2 = make_checkpoint(trace, "node-b", 2);
        let id1 = ck1.checkpoint_id.clone();
        let id2 = ck2.checkpoint_id.clone();

        store.save(&ck1).await.unwrap();
        store.save(&ck2).await.unwrap();

        let ids = store.list(&trace).await.unwrap();
        assert_eq!(ids.len(), 2);
        // 倒序 — 最新的在前
        assert_eq!(ids[0], id2);
        assert_eq!(ids[1], id1);
    }

    #[tokio::test]
    async fn test_delete() {
        let store = InMemoryCheckpointStore::new();
        let trace = TraceId::new();
        let ck = make_checkpoint(trace, "node-a", 1);
        let id = ck.checkpoint_id.clone();

        store.save(&ck).await.unwrap();
        assert!(store.delete(&id).await.unwrap());

        assert!(store.load(&id).await.unwrap().is_none());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let store = InMemoryCheckpointStore::new();
        let fake_id = CheckpointId(uuid::Uuid::new_v4());

        let result = store.delete(&fake_id).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_prune() {
        let store = InMemoryCheckpointStore::new();
        let trace = TraceId::new();

        for i in 1..=5 {
            store
                .save(&make_checkpoint(trace, "node", i))
                .await
                .unwrap();
        }

        assert_eq!(store.len(), 5);

        let deleted = store.prune(&trace, 2).await.unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(store.len(), 2);

        // 保留的是最新的两个
        let latest = store
            .load_latest(&trace)
            .await
            .unwrap()
            .expect("should exist");
        assert_eq!(latest.get_state_value("step"), Some(5));
    }

    #[tokio::test]
    async fn test_prune_keep_more_than_exists() {
        let store = InMemoryCheckpointStore::new();
        let trace = TraceId::new();

        store
            .save(&make_checkpoint(trace, "node", 1))
            .await
            .unwrap();
        store
            .save(&make_checkpoint(trace, "node", 2))
            .await
            .unwrap();

        let deleted = store.prune(&trace, 10).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.len(), 2);
    }

    #[tokio::test]
    async fn test_concurrent_access() {
        let store = std::sync::Arc::new(InMemoryCheckpointStore::new());
        let trace = TraceId::new();

        let mut handles = vec![];

        for i in 0..10 {
            let store_clone = store.clone();
            let handle = tokio::spawn(async move {
                let ck = make_checkpoint(trace, "node", i);
                store_clone.save(&ck).await.unwrap();
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(store.len(), 10);

        let ids = store.list(&trace).await.unwrap();
        assert_eq!(ids.len(), 10);
    }

    #[tokio::test]
    async fn test_load_nonexistent_trace() {
        let store = InMemoryCheckpointStore::new();
        let fake_trace = TraceId::new();

        let result = store.load_latest(&fake_trace).await.unwrap();
        assert!(result.is_none());

        let ids = store.list(&fake_trace).await.unwrap();
        assert!(ids.is_empty());
    }
}

// ─── Helper: Checkpoint 便捷读取 state 值 ──────────────────────

/// Checkpoint 扩展 — 便捷读取物化状态中的值。
///
/// 由于 `Checkpoint.state` 是 `pub` 字段，此 trait 仅提供常用的快捷方法。
pub trait CheckpointExt {
    fn get_state_value(&self, key: &str) -> Option<u64>;
}

impl CheckpointExt for Checkpoint {
    fn get_state_value(&self, key: &str) -> Option<u64> {
        self.state.get(key).and_then(|v| v.as_u64())
    }
}
