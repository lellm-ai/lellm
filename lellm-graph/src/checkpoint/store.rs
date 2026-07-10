//! Checkpoint 存储后端 — BlobCheckpointStore SPI + 内存后端实现。
//!
//! 存储层操作 `CheckpointBlob`（bytes in / bytes out），与 State 类型和序列化格式解耦。

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use super::checkpoint_data::{CheckpointBlob, CheckpointId, CheckpointStoreError, TraceId};

// ─── BlobCheckpointStore Trait ─────────────────────────────────

/// Checkpoint 存储后端 SPI — bytes in / bytes out。
///
/// 存储层无需知道 State 类型或序列化格式，只操作 `CheckpointBlob`。
/// 通过 `TypedCheckpointStore` 组合 Codec 实现类型化的 save/load。
#[async_trait]
pub trait BlobCheckpointStore: Send + Sync {
    /// 保存 CheckpointBlob 并关联 trace_id。
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        blob: &CheckpointBlob,
    ) -> Result<(), CheckpointStoreError>;

    /// 加载指定 ID 的 CheckpointBlob。
    async fn load(&self, id: &CheckpointId)
    -> Result<Option<CheckpointBlob>, CheckpointStoreError>;

    /// 加载 trace 最新的 CheckpointBlob。
    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError>;

    /// 列出 trace 的所有 CheckpointId（按时间倒序）。
    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError>;

    /// 删除指定 ID 的 Checkpoint。
    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError>;

    /// 修剪 trace 的旧 Checkpoint，保留最新的 keep 个。
    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError>;
}

// ─── InMemoryBlobStore ─────────────────────────────────────────

/// 基于内存的 Checkpoint 存储后端。
///
/// 通过 `save_with_trace()` 关联 trace_id，或在存储层组织关联。
#[derive(Default)]
pub struct InMemoryBlobStore {
    store: RwLock<HashMap<CheckpointId, CheckpointBlob>>,
    /// trace_id → [CheckpointId] 索引（按时间正序）
    index: RwLock<HashMap<TraceId, Vec<CheckpointId>>>,
}

impl InMemoryBlobStore {
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
impl BlobCheckpointStore for InMemoryBlobStore {
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        blob: &CheckpointBlob,
    ) -> Result<(), CheckpointStoreError> {
        let id = blob.id.clone();

        {
            let mut store = self
                .store
                .write()
                .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            store.insert(id.clone(), blob.clone());
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

    async fn load(
        &self,
        id: &CheckpointId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        let store = self
            .store
            .read()
            .map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
        Ok(store.get(id).cloned())
    }

    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
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
