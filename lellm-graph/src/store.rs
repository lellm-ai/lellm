//! Checkpoint 存储后端实现 — 内存后端。

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointId, CheckpointStore, CheckpointStoreError, TraceId};

/// 基于内存的 Checkpoint 存储后端。
///
/// 通过 `save_with_trace()` 关联 trace_id，或在存储层组织关联。
#[derive(Default)]
pub struct InMemoryCheckpointStore {
    store: RwLock<HashMap<CheckpointId, Checkpoint>>,
    /// trace_id → [CheckpointId] 索引（按时间正序）
    index: RwLock<HashMap<TraceId, Vec<CheckpointId>>>,
}

impl InMemoryCheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.store.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        checkpoint: &Checkpoint,
    ) -> Result<(), CheckpointStoreError> {
        let id = checkpoint.checkpoint_id.clone();

        {
            let mut store = self
                .store
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            store.insert(id.clone(), checkpoint.clone());
        }

        {
            let mut index = self
                .index
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            index.entry(*trace_id).or_default().push(id);
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
        let ids = index.get(trace_id).cloned().unwrap_or_default();
        Ok(ids.into_iter().rev().collect())
    }

    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
        let mut store = self
            .store
            .write()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        store
            .remove(id)
            .map(|_| true)
            .ok_or_else(|| CheckpointStoreError::Storage("failed to acquire write lock".into()))
    }

    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError> {
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
