//! Checkpoint 恢复测试 — 验证从 Blob 恢复 Checkpoint 的完整链路。
//!
//! 参见: `docs/adr/v04-execution-model-redesign.md` 决策 4 (Phase D)

use lellm_graph::{
    Checkpoint, InMemoryBlobStore, SerdeCheckpointCodec, State, StateExt, TraceId, TypedCheckpointStore,
};

/// 测试完整的保存 → 加载 → 恢复链路
#[tokio::test]
async fn test_checkpoint_restore_roundtrip() {
    let store = InMemoryBlobStore::new();
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&store, codec);

    let trace_id = TraceId::new();

    // 构建带有状态的 Checkpoint
    let mut state = State::new();
    state.insert("user_id".to_string(), serde_json::json!("u123"));
    state.insert("step".to_string(), serde_json::json!(42));

    let cp = Checkpoint::new("process_order", state);
    let cp_id = cp.checkpoint_id.clone();

    // 保存
    typed
        .save_with_trace(&trace_id, &cp)
        .await
        .expect("save should succeed");

    // 模拟恢复场景：从存储中加载
    let restored = typed
        .load(&cp_id)
        .await
        .expect("load should succeed")
        .expect("checkpoint should exist");

    // 验证恢复的数据完整性
    assert_eq!(restored.checkpoint_id, cp_id);
    assert_eq!(restored.current_node.0, "process_order");
    assert_eq!(restored.state.get_str("user_id"), Some("u123"));
    assert_eq!(restored.state.get_i64("step"), Some(42));

    // 验证可以从恢复的节点继续执行
    assert_eq!(restored.current_node.to_string(), "process_order");
}

/// 测试 load_latest 返回最新的 Checkpoint
#[tokio::test]
async fn test_load_latest_checkpoint() {
    let store = InMemoryBlobStore::new();
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&store, codec);

    let trace_id = TraceId::new();

    // 初始时没有 Checkpoint
    let latest = typed
        .load_latest(&trace_id)
        .await
        .expect("load_latest should succeed");
    assert!(latest.is_none());

    // 保存第一个 Checkpoint
    let cp1 = Checkpoint::new("node_a", State::new());
    typed.save_with_trace(&trace_id, &cp1).await.expect("save cp1");

    // 保存第二个 Checkpoint
    let cp2 = Checkpoint::new("node_b", State::new());
    typed.save_with_trace(&trace_id, &cp2).await.expect("save cp2");

    // load_latest 应返回最新的（cp2）
    let latest = typed
        .load_latest(&trace_id)
        .await
        .expect("load_latest should succeed")
        .expect("should have latest checkpoint");
    assert_eq!(latest.checkpoint_id, cp2.checkpoint_id);
}

/// 测试不同 trace_id 的隔离性
#[tokio::test]
async fn test_trace_isolation() {
    let store = InMemoryBlobStore::new();
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&store, codec);

    let trace_a = TraceId::new();
    let trace_b = TraceId::new();

    let cp_a = Checkpoint::new("node_a", State::new());
    typed.save_with_trace(&trace_a, &cp_a).await.expect("save cp_a");

    let cp_b = Checkpoint::new("node_b", State::new());
    typed.save_with_trace(&trace_b, &cp_b).await.expect("save cp_b");

    // trace_a 的 latest 应该是 cp_a
    let latest_a = typed.load_latest(&trace_a).await.expect("load_latest trace_a");
    assert!(latest_a.is_some());
    assert_eq!(latest_a.unwrap().checkpoint_id, cp_a.checkpoint_id);

    // trace_b 的 latest 应该是 cp_b
    let latest_b = typed.load_latest(&trace_b).await.expect("load_latest trace_b");
    assert!(latest_b.is_some());
    assert_eq!(latest_b.unwrap().checkpoint_id, cp_b.checkpoint_id);
}
