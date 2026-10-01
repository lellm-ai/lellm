//! CheckpointCodec — 序列化层，对象 ↔ 二进制表示。
//!
//! 将 `Checkpoint<S>` 序列化为 `CheckpointBlob`，实现存储层与 State 类型的解耦。
//!
//! # 设计
//!
//! ```text
//! Checkpoint<S> ──serialize()──▶ CheckpointBlob ──deserialize()──▶ Checkpoint<S>
//! ```
//!
//! Codec 实现可以选择任意序列化格式（JSON、MessagePack、Bincode 等），
//! 存储层只需操作 `CheckpointBlob`，无需知道 State 类型或序列化格式。

use serde::{Deserialize, Serialize};

use super::checkpoint_data::{Checkpoint, CheckpointBlob, CheckpointStoreError};
use super::store::BlobCheckpointStore;
use crate::state::State;
use crate::state::workflow_state::WorkflowState;

// ─── CheckpointCodec Trait ─────────────────────────────────────

/// Checkpoint 序列化/反序列化接口。
///
/// # 泛型参数
///
/// - `S` — 类型化状态（默认 `State` = HashMap，向后兼容）
pub trait CheckpointCodec<S: WorkflowState = State>: Send + Sync {
    /// 将 Checkpoint 序列化为二进制 Blob。
    ///
    /// `graph_hash` 由调用方提供（从 `Graph::hash_u64()` 获取），
    /// 写入 Blob 作为 correctness invariant。
    fn serialize(
        &self,
        cp: &Checkpoint<S>,
        graph_hash: u64,
    ) -> Result<CheckpointBlob, CheckpointStoreError>;

    /// 从二进制 Blob 反序列化为 Checkpoint。
    ///
    /// 如果 Blob 中的 `graph_hash` 与 `expected_hash` 不匹配，
    /// 返回 `CheckpointStoreError::GraphMismatch`。
    fn deserialize(
        &self,
        blob: &CheckpointBlob,
        expected_hash: u64,
    ) -> Result<Checkpoint<S>, CheckpointStoreError>;
}

// ─── SerdeCheckpointCodec ──────────────────────────────────────

/// 基于 Serde + JSON 的默认 Codec 实现。
///
/// 使用 `serde_json` 进行序列化，适用于大多数场景。
/// 对于性能敏感场景，可替换为 Bincode 或 MessagePack。
#[derive(Debug, Default, Clone)]
pub struct SerdeCheckpointCodec<S: WorkflowState = State> {
    _phantom: std::marker::PhantomData<S>,
}

impl<S: WorkflowState> SerdeCheckpointCodec<S> {
    pub fn new() -> Self {
        Self {
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<S> CheckpointCodec<S> for SerdeCheckpointCodec<S>
where
    S: WorkflowState + Serialize + for<'de> Deserialize<'de>,
{
    fn serialize(
        &self,
        cp: &Checkpoint<S>,
        graph_hash: u64,
    ) -> Result<CheckpointBlob, CheckpointStoreError> {
        let data = serde_json::to_vec(cp)
            .map_err(|e| CheckpointStoreError::Serialization(e.to_string()))?;
        Ok(CheckpointBlob::new(
            cp.checkpoint_id.clone(),
            data,
            graph_hash,
            cp.created_at,
        ))
    }

    fn deserialize(
        &self,
        blob: &CheckpointBlob,
        expected_hash: u64,
    ) -> Result<Checkpoint<S>, CheckpointStoreError> {
        if blob.graph_hash != expected_hash {
            return Err(CheckpointStoreError::GraphMismatch {
                expected: expected_hash,
                actual: blob.graph_hash,
            });
        }

        // 段 1：语法 — 解析为通用 Value
        let value: serde_json::Value = serde_json::from_slice(&blob.data)
            .map_err(|e| CheckpointStoreError::Corrupted(e.to_string()))?;
        if !value.is_object() {
            return Err(CheckpointStoreError::Corrupted(
                "checkpoint is not a JSON object".into(),
            ));
        }

        // 段 2：格式校验 — 对 Value 做结构化检查（三态在此天然可分：
        // 键缺失 ≠ Value::Null ≠ 值）
        validate_checkpoint_format(&value)?;

        // 段 3：类型化 — 类型不符 → Corrupted
        serde_json::from_value(value).map_err(|e| CheckpointStoreError::Corrupted(e.to_string()))
    }
}

/// 段 2 格式校验 — 缺失键 / 版本不符 → UnsupportedFormat（不靠错误文本分类）。
///
/// - 缺 `format_version` 或版本不支持 → `UnsupportedFormat`（legacy 格式）
/// - 缺 `next_node` 键 → `UnsupportedFormat`（禁止解释为已完成）
/// - 显式 `"next_node": null` 合法（已完成）
pub(crate) fn validate_checkpoint_format(
    v: &serde_json::Value,
) -> Result<(), CheckpointStoreError> {
    let fmt_version = v.get("format_version").ok_or_else(|| {
        CheckpointStoreError::UnsupportedFormat("missing format_version (legacy format?)".into())
    })?;
    let expected = super::checkpoint_data::CHECKPOINT_FORMAT_VERSION as u64;
    fmt_version
        .as_u64()
        .filter(|x| *x == expected)
        .ok_or_else(|| {
            CheckpointStoreError::UnsupportedFormat(format!(
                "unsupported format_version: {fmt_version}"
            ))
        })?;
    // next_node 键必须存在：缺失 = legacy（拒绝）；null = 已完成（合法）
    if v.get("next_node").is_none() {
        return Err(CheckpointStoreError::UnsupportedFormat(
            "missing next_node (legacy current_node format?)".into(),
        ));
    }
    Ok(())
}

// ─── TypedCheckpointStore ──────────────────────────────────────

/// 类型化 Checkpoint 存储 — Codec + BlobStore 的组合。
///
/// 将 `Checkpoint<S>` 的保存/加载委托给 Codec 进行序列化，
/// 再通过 BlobCheckpointStore 进行持久化。
///
/// # 示例
///
/// ```rust,ignore
/// let store = InMemoryBlobStore::new();
/// let codec = SerdeCheckpointCodec::<State>::new();
/// let typed = TypedCheckpointStore::new(&store, codec);
///
/// typed.save_with_trace(&trace_id, &checkpoint, graph_hash).await?;
/// let restored = typed.load(&id, graph_hash).await?;
/// ```
pub struct TypedCheckpointStore<'a, Codec, S: WorkflowState = State> {
    store: &'a dyn BlobCheckpointStore,
    codec: Codec,
    _phantom: std::marker::PhantomData<S>,
}

impl<'a, Codec, S> TypedCheckpointStore<'a, Codec, S>
where
    S: WorkflowState,
{
    pub fn new(store: &'a dyn BlobCheckpointStore, codec: Codec) -> Self {
        Self {
            store,
            codec,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<'a, Codec, S> TypedCheckpointStore<'a, Codec, S>
where
    S: WorkflowState + Serialize + for<'de> Deserialize<'de>,
    Codec: CheckpointCodec<S>,
{
    /// 保存 Checkpoint 并关联 trace_id。
    ///
    /// `graph_hash` 由调用方提供（从 `Graph::hash_u64()` 获取），
    /// 写入 Blob 作为 correctness invariant。
    pub async fn save_with_trace(
        &self,
        trace_id: &super::checkpoint_data::TraceId,
        checkpoint: &Checkpoint<S>,
        graph_hash: u64,
    ) -> Result<(), CheckpointStoreError> {
        let blob = self.codec.serialize(checkpoint, graph_hash)?;
        self.store.save_with_trace(trace_id, &blob).await
    }

    /// 加载指定 ID 的 Checkpoint。
    ///
    /// 校验 `graph_hash`：不匹配则返回 `GraphMismatch` 错误。
    pub async fn load(
        &self,
        id: &super::checkpoint_data::CheckpointId,
        expected_hash: u64,
    ) -> Result<Option<Checkpoint<S>>, CheckpointStoreError> {
        match self.store.load(id).await? {
            Some(blob) => Ok(Some(self.codec.deserialize(&blob, expected_hash)?)),
            None => Ok(None),
        }
    }

    /// 加载 trace 最新的 Checkpoint。
    ///
    /// 校验 `graph_hash`：不匹配则返回 `GraphMismatch` 错误。
    pub async fn load_latest(
        &self,
        trace_id: &super::checkpoint_data::TraceId,
        expected_hash: u64,
    ) -> Result<Option<Checkpoint<S>>, CheckpointStoreError> {
        match self.store.load_latest(trace_id).await? {
            Some(blob) => Ok(Some(self.codec.deserialize(&blob, expected_hash)?)),
            None => Ok(None),
        }
    }
}
