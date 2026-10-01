//! Checkpoint 存储后端 — BlobCheckpointStore SPI + 内存/文件后端实现。
//!
//! 存储层操作 `CheckpointBlob`（bytes in / bytes out），与 State 类型和序列化格式解耦。

use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

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
///
/// 内部使用单个 RwLock 保护 store + index，确保原子性。
#[derive(Default)]
pub struct InMemoryBlobStore {
    inner: RwLock<InMemoryBlobStoreInner>,
}

#[derive(Default)]
struct InMemoryBlobStoreInner {
    store: HashMap<CheckpointId, CheckpointBlob>,
    /// trace_id → [CheckpointId] 索引（按时间正序）
    index: HashMap<TraceId, Vec<CheckpointId>>,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.read().store.len()
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
        let mut inner = self.inner.write();
        inner.store.insert(id.clone(), blob.clone());
        inner.index.entry(*trace_id).or_default().push(id);
        Ok(())
    }

    async fn load(
        &self,
        id: &CheckpointId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        let inner = self.inner.read();
        Ok(inner.store.get(id).cloned())
    }

    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        let inner = self.inner.read();
        let last_id = inner
            .index
            .get(trace_id)
            .and_then(|ids| ids.last())
            .cloned();
        match last_id {
            Some(id) => Ok(inner.store.get(&id).cloned()),
            None => Ok(None),
        }
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError> {
        let inner = self.inner.read();
        let ids = inner.index.get(trace_id).cloned().unwrap_or_default();
        Ok(ids.into_iter().rev().collect())
    }

    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
        let mut inner = self.inner.write();
        Ok(inner.store.remove(id).is_some())
    }

    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError> {
        let mut inner = self.inner.write();
        let to_delete: Vec<CheckpointId> = match inner.index.get_mut(trace_id) {
            Some(ids) if ids.len() > keep => {
                let remove_count = ids.len() - keep;
                ids.drain(..remove_count).collect()
            }
            _ => return Ok(0),
        };
        for id in &to_delete {
            inner.store.remove(id);
        }
        Ok(to_delete.len())
    }
}

// ─── FileBlobStore ─────────────────────────────────────────────

/// 文件 Checkpoint 存储后端。
///
/// # 布局
///
/// ```text
/// <root>/<trace_id>/<seq>_<checkpoint_id>        # 已提交（原子可见）
/// <root>/<trace_id>/<seq>_<checkpoint_id>.tmp    # 写入中（崩溃残留，load 时忽略）
/// ```
///
/// - `seq` = trace 内**单调递增的提交序号**：保存时扫 trace 目录取当前最大 seq + 1
///   （目录即记录，无独立计数器文件，进程崩溃后自动延续）。
///   **按数值排序**（非文件名字典序）；`.tmp` 与非规范文件不参与。
/// - `created_at` 仅展示用途，不参与排序（墙上时钟可相同、可因校时倒退）。
///
/// # 写入协议
///
/// `write_all(信封) → flush().await → rename(.tmp → 最终名)`。
/// flush 确保写入完成（tokio::fs 写返回不代表完成）；同目录 rename 原子。
/// **落盘边界**：保证写入完成 + 原子可见（进程崩溃安全：panic/SIGKILL），
/// **不保证断电/OS 崩溃**（需文件 + 目录 fsync，phase 2 评估）。
///
/// # 单写者约束
///
/// **同一 trace 同时只能有一个活跃执行者**——seq 的「扫目录取 max+1」
/// 在并发写者下会取到同一 seq 而冲突。phase 1 不加锁；多执行者分叉写入不支持。
pub struct FileBlobStore {
    root: PathBuf,
}

/// 信封 — 文件内容（data 为 base64，对任意 Codec 字节通用）。
#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    id: String,
    graph_hash: u64,
    created_at_nanos: u64,
    data: String,
}

impl FileBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn trace_dir(&self, trace_id: &TraceId) -> PathBuf {
        self.root.join(trace_id.to_string())
    }

    /// 扫描 trace 目录 → (seq, path, id)。跳过 `.tmp` 与非规范文件。
    fn scan_trace_dir(dir: &Path) -> Result<Vec<(u64, PathBuf, String)>, CheckpointStoreError> {
        let mut out = Vec::new();
        let rd = std::fs::read_dir(dir).map_err(|e| {
            CheckpointStoreError::Storage(format!("read trace dir {}: {e}", dir.display()))
        })?;
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".tmp") {
                continue; // 临时文件不参与
            }
            let Some(usize_pos) = name.find('_') else {
                continue; // 非规范文件
            };
            let Ok(seq) = name[..usize_pos].parse::<u64>() else {
                continue; // 前缀非数字 = 非规范
            };
            out.push((seq, entry.path(), name[usize_pos + 1..].to_string()));
        }
        Ok(out)
    }

    async fn read_blob(path: &Path) -> Result<CheckpointBlob, CheckpointStoreError> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("read {}: {e}", path.display())))?;
        let env: Envelope = serde_json::from_slice(&bytes)
            .map_err(|e| CheckpointStoreError::Corrupted(format!("envelope: {e}")))?;
        let data =
            base64::engine::Engine::decode(&base64::engine::general_purpose::STANDARD, &env.data)
                .map_err(|e| CheckpointStoreError::Corrupted(format!("base64: {e}")))?;
        let id = CheckpointId(
            env.id
                .parse::<sparkid::SparkId>()
                .map_err(|e| CheckpointStoreError::Corrupted(format!("id: {e}")))?,
        );
        let created_at =
            std::time::UNIX_EPOCH + std::time::Duration::from_nanos(env.created_at_nanos);
        Ok(CheckpointBlob::new(id, data, env.graph_hash, created_at))
    }
}

#[async_trait]
impl BlobCheckpointStore for FileBlobStore {
    async fn save_with_trace(
        &self,
        trace_id: &TraceId,
        blob: &CheckpointBlob,
    ) -> Result<(), CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("create dir: {e}")))?;

        let entries = Self::scan_trace_dir(&dir)?;
        let next_seq = entries
            .iter()
            .map(|(s, _, _)| *s)
            .max()
            .map(|m| {
                m.checked_add(1)
                    .ok_or_else(|| CheckpointStoreError::Storage("checkpoint seq overflow".into()))
            })
            .transpose()?
            .unwrap_or(1);

        let id_str = blob.id.to_string();
        let final_path = dir.join(format!("{next_seq}_{id_str}"));
        let tmp_path = dir.join(format!("{next_seq}_{id_str}.tmp"));

        let created_at_nanos = blob
            .created_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let env = Envelope {
            id: id_str,
            graph_hash: blob.graph_hash,
            created_at_nanos,
            data: base64::engine::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &blob.data,
            ),
        };
        let json = serde_json::to_vec(&env)
            .map_err(|e| CheckpointStoreError::Serialization(e.to_string()))?;

        let mut file = tokio::fs::File::create(&tmp_path)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("create tmp: {e}")))?;
        file.write_all(&json)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("write: {e}")))?;
        file.flush()
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("flush: {e}")))?;
        // 注：此处不做 fsync — 进程崩溃安全，非断电安全（见类型 doc）
        tokio::fs::rename(&tmp_path, &final_path)
            .await
            .map_err(|e| CheckpointStoreError::Storage(format!("rename: {e}")))?;
        Ok(())
    }

    async fn load(
        &self,
        id: &CheckpointId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        // SPI 的 load(id) 不带 trace 上下文 — sparkid 全局唯一，扫全部 trace 目录
        let id_str = id.to_string();
        let mut traces = std::fs::read_dir(&self.root)
            .map_err(|e| CheckpointStoreError::Storage(format!("read root: {e}")))?;
        while let Some(entry) = traces.next() {
            let entry = entry.map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            if !entry.path().is_dir() {
                continue;
            }
            let entries = Self::scan_trace_dir(&entry.path())?;
            if let Some((_, path, _)) = entries.into_iter().find(|(_, _, eid)| *eid == id_str) {
                return Ok(Some(Self::read_blob(&path).await?));
            }
        }
        Ok(None)
    }

    async fn load_latest(
        &self,
        trace_id: &TraceId,
    ) -> Result<Option<CheckpointBlob>, CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        if !dir.exists() {
            return Ok(None);
        }
        let entries = Self::scan_trace_dir(&dir)?;
        // 数值最大 seq = 最新；损坏 → Corrupted（不回退旧检查点 = 不悄悄重跑）
        match entries.into_iter().max_by_key(|(seq, _, _)| *seq) {
            Some((_, path, _)) => Ok(Some(Self::read_blob(&path).await?)),
            None => Ok(None),
        }
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<CheckpointId>, CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries = Self::scan_trace_dir(&dir)?;
        entries.sort_by(|a, b| b.0.cmp(&a.0)); // seq 倒序
        entries
            .into_iter()
            .map(|(_, _, id)| {
                id.parse::<sparkid::SparkId>()
                    .map(CheckpointId)
                    .map_err(|e| CheckpointStoreError::Corrupted(format!("id: {e}")))
            })
            .collect()
    }

    async fn delete(&self, id: &CheckpointId) -> Result<bool, CheckpointStoreError> {
        let id_str = id.to_string();
        let mut traces = std::fs::read_dir(&self.root)
            .map_err(|e| CheckpointStoreError::Storage(format!("read root: {e}")))?;
        while let Some(entry) = traces.next() {
            let entry = entry.map_err(|e| CheckpointStoreError::Storage(e.to_string()))?;
            if !entry.path().is_dir() {
                continue;
            }
            let entries = Self::scan_trace_dir(&entry.path())?;
            if let Some((_, path, _)) = entries.into_iter().find(|(_, _, eid)| *eid == id_str) {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| CheckpointStoreError::Storage(format!("delete: {e}")))?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn prune(&self, trace_id: &TraceId, keep: usize) -> Result<usize, CheckpointStoreError> {
        let dir = self.trace_dir(trace_id);
        if !dir.exists() {
            return Ok(0);
        }
        let mut entries = Self::scan_trace_dir(&dir)?;
        if entries.len() <= keep {
            return Ok(0);
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0)); // seq 倒序，保留前 keep 个
        let mut deleted = 0;
        for (_, path, _) in &entries[keep..] {
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| CheckpointStoreError::Storage(format!("prune: {e}")))?;
            deleted += 1;
        }
        Ok(deleted)
    }
}
