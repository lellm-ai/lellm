//! 恢复能力第一阶段 — 格式与入口校验测试（T4 / T7b / roundtrip）。

use lellm_graph::{
    CHECKPOINT_FORMAT_VERSION, Checkpoint, CheckpointCodec, CheckpointId, CheckpointStoreError,
    NodeId, SerdeCheckpointCodec, State,
};

const HASH: u64 = 0x1234_5678_9abc_def0;

/// 新格式 roundtrip：format_version / next_node / steps_used 全部保留
#[tokio::test]
async fn test_checkpoint_v1_roundtrip() {
    let codec = SerdeCheckpointCodec::<State>::new();
    let state = State::new();
    let cp = Checkpoint::new(Some(NodeId("b".into())), &state, HASH, 3);
    assert_eq!(cp.format_version, CHECKPOINT_FORMAT_VERSION);
    assert_eq!(cp.steps_used, 3);

    let blob = codec.serialize(&cp, HASH).expect("serialize");
    let restored = codec.deserialize(&blob, HASH).expect("deserialize");
    assert_eq!(restored.format_version, CHECKPOINT_FORMAT_VERSION);
    assert_eq!(restored.next_node, Some(NodeId("b".into())));
    assert_eq!(restored.steps_used, 3);
    assert_eq!(restored.checkpoint_id, cp.checkpoint_id);
}

/// CheckpointId 使用 sparkid：21 字符、可解析回
#[test]
fn test_checkpoint_id_sparkid() {
    let id = CheckpointId::new();
    let s = id.to_string();
    assert_eq!(s.len(), 21);
    let parsed: sparkid::SparkId = s.parse().expect("parse back");
    assert_eq!(id.0, parsed);
}

/// T4: 严格加载 7 用例 — Codec 两段式（不用错误文本分类）
#[tokio::test]
async fn t4_strict_loading_rejects_legacy_and_missing() {
    use lellm_graph::CheckpointBlob;
    use serde_json::json;
    use std::time::SystemTime;

    let codec = SerdeCheckpointCodec::<State>::new();

    // 基准：真实序列化的 v1 checkpoint（含合法 SystemTime 编码）
    let base = serde_json::to_value(Checkpoint::new(
        Some(NodeId("a".into())),
        &State::new(),
        HASH,
        1,
    ))
    .expect("serialize base");

    let make_blob = |v: serde_json::Value| {
        CheckpointBlob::new(
            CheckpointId::new(),
            serde_json::to_vec(&v).expect("blob data"),
            HASH,
            SystemTime::now(),
        )
    };

    // 1. JSON 语法损坏 → Corrupted
    let bad = CheckpointBlob::new(
        CheckpointId::new(),
        b"{ not json".to_vec(),
        HASH,
        SystemTime::now(),
    );
    match codec.deserialize(&bad, HASH) {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("case1 expected Corrupted, got: {other:?}"),
    }

    // 2. 顶层非对象 → Corrupted
    let not_obj = CheckpointBlob::new(
        CheckpointId::new(),
        b"[1,2]".to_vec(),
        HASH,
        SystemTime::now(),
    );
    match codec.deserialize(&not_obj, HASH) {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("case2 expected Corrupted, got: {other:?}"),
    }

    // 3. 缺 format_version → UnsupportedFormat
    let mut v = base.clone();
    v.as_object_mut().expect("obj").remove("format_version");
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case3 expected UnsupportedFormat, got: {other:?}"),
    }

    // 4. format_version=999 → UnsupportedFormat
    let mut v = base.clone();
    v["format_version"] = json!(999);
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case4 expected UnsupportedFormat, got: {other:?}"),
    }

    // 5. 缺 next_node 键 → UnsupportedFormat（禁止解释为已完成）
    let mut v = base.clone();
    v.as_object_mut().expect("obj").remove("next_node");
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case5 expected UnsupportedFormat, got: {other:?}"),
    }

    // 6. 显式 "next_node": null → 合法，已完成
    let mut v = base.clone();
    v["next_node"] = serde_json::Value::Null;
    let cp = codec
        .deserialize(&make_blob(v), HASH)
        .expect("case6 should load");
    assert_eq!(cp.next_node, None, "explicit null = completed");

    // 7. 旧格式（current_node、无 format_version）→ UnsupportedFormat
    let legacy = json!({
        "checkpoint_id": CheckpointId::new().to_string(),
        "current_node": "a",
        "state": {},
        "graph_hash": HASH,
        "created_at": base["created_at"].clone(),
    });
    match codec.deserialize(&make_blob(legacy), HASH) {
        Err(CheckpointStoreError::UnsupportedFormat(_)) => {}
        other => panic!("case7 expected UnsupportedFormat, got: {other:?}"),
    }

    // 8. 段 2 通过但类型不符（next_node 是数字）→ Corrupted
    let mut v = base.clone();
    v["next_node"] = json!(42);
    match codec.deserialize(&make_blob(v), HASH) {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("case8 expected Corrupted, got: {other:?}"),
    }
}
