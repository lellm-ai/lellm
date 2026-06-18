//! Checkpoint 存储后端实现 — 从 lellm-runtime 合并。

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::checkpoint::{Checkpoint, CheckpointId, CheckpointStore, CheckpointStoreError};
use crate::ids::TraceId;

/// 基于内存的 Checkpoint 存储后端。
#[derive(Default)]
pub struct InMemoryCheckpointStore {
    store: RwLock<HashMap<CheckpointId, Checkpoint>>,
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
    async fn save(&self, checkpoint: &Checkpoint) -> Result<(), CheckpointStoreError> {
        let ck = checkpoint.clone();
        let id = ck.checkpoint_id.clone();
        let trace = ck.parent_trace_id;

        {
            let mut store = self
                .store
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            store.insert(id.clone(), ck);
        }

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
        let trace_id = {
            let mut store = self
                .store
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            store.remove(id).map(|ck| ck.parent_trace_id)
        };

        match trace_id {
            Some(trace) => {
                let mut index = self
                    .index
                    .write()
                    .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
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

/// Checkpoint 扩展 — 便捷读取物化状态中的值。
pub trait CheckpointExt {
    fn get_state_value(&self, key: &str) -> Option<u64>;
}

impl CheckpointExt for Checkpoint {
    fn get_state_value(&self, key: &str) -> Option<u64> {
        self.state.get(key).and_then(|v| v.as_u64())
    }
}
