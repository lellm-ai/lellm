//! Checkpoint 分层架构集成测试。
//!
//! 验证 Checkpoint → Codec → Blob → Store 分层链路。
//!
//! 参见: `docs/adr/v04-execution-model-redesign.md` 决策 4 (Phase D)

use lellm_graph::{
    BlobCheckpointStore, Checkpoint, CheckpointCodec, CheckpointId, CheckpointStoreError,
    InMemoryBlobStore, SerdeCheckpointCodec, State, TraceId, TriggerPolicy, TypedCheckpointStore,
};
use uuid::Uuid;

const TEST_GRAPH_HASH: u64 = 0x1234_5678_9abc_def0;

/// 测试 SerdeCheckpointCodec 序列化/反序列化
#[tokio::test]
async fn test_serde_codec_roundtrip() {
    let codec = SerdeCheckpointCodec::<State>::new();
    let state = State::new();
    let cp = Checkpoint::new("test_node", state, TEST_GRAPH_HASH);

    let blob = codec
        .serialize(&cp, TEST_GRAPH_HASH)
        .expect("serialize should succeed");
    assert!(!blob.data.is_empty(), "serialized data should not be empty");
    assert_eq!(blob.id, cp.checkpoint_id);
    assert_eq!(blob.graph_hash, TEST_GRAPH_HASH);

    let restored = codec
        .deserialize(&blob, TEST_GRAPH_HASH)
        .expect("deserialize should succeed");
    assert_eq!(restored.checkpoint_id, cp.checkpoint_id);
    assert_eq!(restored.current_node, cp.current_node);
}

/// 测试 graph_hash 不匹配时 deserialize 返回 GraphMismatch
#[tokio::test]
async fn test_graph_hash_mismatch_rejected() {
    let codec = SerdeCheckpointCodec::<State>::new();
    let state = State::new();
    let cp = Checkpoint::new("test_node", state, TEST_GRAPH_HASH);

    let blob = codec
        .serialize(&cp, TEST_GRAPH_HASH)
        .expect("serialize should succeed");

    let wrong_hash = TEST_GRAPH_HASH ^ 0xFF;
    let result = codec.deserialize(&blob, wrong_hash);
    assert!(result.is_err());
    match result.unwrap_err() {
        CheckpointStoreError::GraphMismatch { expected, actual } => {
            assert_eq!(expected, wrong_hash);
            assert_eq!(actual, TEST_GRAPH_HASH);
        }
        other => panic!("expected GraphMismatch, got: {other}"),
    }
}

/// 测试 TypedCheckpointStore 保存与加载
#[tokio::test]
async fn test_typed_store_save_and_load() {
    let store = InMemoryBlobStore::new();
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&store, codec);

    let trace_id = TraceId::new();
    let state = State::new();
    let cp = Checkpoint::new("start", state, TEST_GRAPH_HASH);
    let cp_id = cp.checkpoint_id.clone();

    typed
        .save_with_trace(&trace_id, &cp, TEST_GRAPH_HASH)
        .await
        .expect("save should succeed");

    let loaded = typed
        .load(&cp_id, TEST_GRAPH_HASH)
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
    let blob = CheckpointBlob::new(
        id.clone(),
        vec![1, 2, 3],
        TEST_GRAPH_HASH,
        SystemTime::now(),
    );

    assert_eq!(blob.id, id);
    assert_eq!(blob.data, vec![1, 2, 3]);
    assert_eq!(blob.graph_hash, TEST_GRAPH_HASH);
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
    let blob = CheckpointBlob::new(
        id.clone(),
        vec![1, 2, 3],
        TEST_GRAPH_HASH,
        SystemTime::now(),
    );

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

/// 测试 TriggerPolicy 枚举
#[test]
fn test_checkpoint_policy() {
    let default_policy = TriggerPolicy::default();
    assert_eq!(default_policy, TriggerPolicy::EveryNode);
    assert_eq!(TriggerPolicy::BarrierOnly, TriggerPolicy::BarrierOnly);
    assert_eq!(TriggerPolicy::Manual, TriggerPolicy::Manual);
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

    let graph_mismatch = CheckpointStoreError::GraphMismatch {
        expected: 0xAAAA,
        actual: 0xBBBB,
    };
    let msg = format!("{graph_mismatch}");
    assert!(msg.contains("graph mismatch"));
    assert!(msg.contains("0x000000000000aaaa"));
    assert!(msg.contains("0x000000000000bbbb"));
}
