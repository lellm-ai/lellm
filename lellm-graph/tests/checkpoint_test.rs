//! Checkpoint 分层架构集成测试。
//!
//! 验证 Checkpoint → Codec → Blob → Store 分层链路。
//!
//! 参见: `docs/adr/v04-execution-model-redesign.md` 决策 4 (Phase D)

use lellm_graph::{
    Checkpoint, CheckpointCodec, CheckpointId, CheckpointPolicy, CheckpointStoreError,
    SerdeCheckpointCodec, State, TypedCheckpointStore, BlobCheckpointStore,
    InMemoryBlobStore, TraceId,
};
use uuid::Uuid;

/// 测试 SerdeCheckpointCodec 序列化/反序列化
#[tokio::test]
async fn test_serde_codec_roundtrip() {
    let codec = SerdeCheckpointCodec::<State>::new();
    let state = State::new();
    let cp = Checkpoint::new("test_node", state);

    let blob = codec.serialize(&cp).expect("serialize should succeed");
    assert!(!blob.data.is_empty(), "serialized data should not be empty");
    assert_eq!(blob.id, cp.checkpoint_id);

    let restored = codec.deserialize(&blob).expect("deserialize should succeed");
    assert_eq!(restored.checkpoint_id, cp.checkpoint_id);
    assert_eq!(restored.current_node, cp.current_node);
}

/// 测试 TypedCheckpointStore 保存与加载
#[tokio::test]
async fn test_typed_store_save_and_load() {
    let store = InMemoryBlobStore::new();
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&store, codec);

    let trace_id = TraceId::new();
    let state = State::new();
    let cp = Checkpoint::new("start", state);
    let cp_id = cp.checkpoint_id.clone();

    typed
        .save_with_trace(&trace_id, &cp)
        .await
        .expect("save should succeed");

    let loaded = typed
        .load(&cp_id)
        .await
        .expect("load should succeed")
        .expect("checkpoint should exist");

    assert_eq!(loaded.checkpoint_id, cp_id);
    assert_eq!(loaded.current_node, cp.current_node);
}

/// 测试 CheckpointBlob 结构
#[test]
fn test_checkpoint_blob_structure() {
    use lellm_graph::CheckpointBlob;
    use std::time::SystemTime;

    let id = CheckpointId(Uuid::new_v4());
    let blob = CheckpointBlob::new(id.clone(), vec![1, 2, 3], SystemTime::now());

    assert_eq!(blob.id, id);
    assert_eq!(blob.data, vec![1, 2, 3]);
}

/// 测试 InMemoryBlobStore 基础操作
#[tokio::test]
async fn test_blob_store_operations() {
    use lellm_graph::CheckpointBlob;
    use std::time::SystemTime;

    let store = InMemoryBlobStore::new();
    let trace_id = TraceId::new();

    assert!(store.is_empty());
    assert_eq!(store.len(), 0);

    let id = CheckpointId(Uuid::new_v4());
    let blob = CheckpointBlob::new(id.clone(), vec![1, 2, 3], SystemTime::now());

    store
        .save_with_trace(&trace_id, &blob)
        .await
        .expect("save should succeed");

    assert_eq!(store.len(), 1);

    let loaded = store.load(&id).await.expect("load should succeed");
    assert!(loaded.is_some());
    assert_eq!(loaded.unwrap().data, vec![1, 2, 3]);

    let ids = store.list(&trace_id).await.expect("list should succeed");
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0], id);

    let deleted = store.delete(&id).await.expect("delete should succeed");
    assert!(deleted);
    assert_eq!(store.len(), 0);
}

/// 测试 CheckpointPolicy 枚举
#[test]
fn test_checkpoint_policy() {
    let default_policy = CheckpointPolicy::default();
    assert_eq!(default_policy, CheckpointPolicy::EveryNode);
    assert_eq!(CheckpointPolicy::BarrierOnly, CheckpointPolicy::BarrierOnly);
    assert_eq!(CheckpointPolicy::Manual, CheckpointPolicy::Manual);
}

/// 测试 CheckpointStoreError 变体
#[test]
fn test_checkpoint_error_variants() {
    let storage_err = CheckpointStoreError::Storage("disk full".into());
    assert!(format!("{storage_err}").contains("disk full"));

    let not_found = CheckpointStoreError::NotFound(CheckpointId(Uuid::nil()));
    assert!(format!("{not_found}").contains("not found"));

    let corrupted = CheckpointStoreError::Corrupted("invalid json".into());
    assert!(format!("{corrupted}").contains("invalid json"));

    let serialization = CheckpointStoreError::Serialization("encode error".into());
    assert!(format!("{serialization}").contains("encode error"));
}
