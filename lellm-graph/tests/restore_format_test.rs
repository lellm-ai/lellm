//! 恢复能力第一阶段 — 格式与入口校验测试（T4 / T7b / roundtrip）。

use lellm_graph::{
    CHECKPOINT_FORMAT_VERSION, Checkpoint, CheckpointCodec, CheckpointId, NodeId,
    SerdeCheckpointCodec, State,
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
