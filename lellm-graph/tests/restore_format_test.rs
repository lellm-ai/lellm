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

/// 消费事件流直至 GraphComplete/GraphError，返回 (trace_id, 结果)。
///
/// 与 restore_test.rs 的 `drain` 同实现（测试文件独立，选择复制）。
async fn drain_stream(
    mut stream: lellm_graph::GraphStream,
) -> (lellm_graph::TraceId, Result<State, String>) {
    let mut trace_id = None;
    loop {
        match stream.recv().await {
            Some(lellm_graph::GraphEvent::GraphStart { trace_id: t }) => trace_id = Some(t),
            Some(lellm_graph::GraphEvent::GraphComplete { result }) => {
                return (trace_id.expect("GraphStart"), Ok(result.state));
            }
            Some(lellm_graph::GraphEvent::GraphError { error, .. }) => {
                return (trace_id.expect("GraphStart"), Err(error.to_string()));
            }
            Some(_) => {}
            None => panic!("stream closed without terminal event"),
        }
    }
}

/// expect_err 等价 — `GraphExecution` 未实现 Debug，标准 `expect_err` 不可用。
fn expect_err<T, E>(result: Result<T, E>, msg: &str) -> E {
    match result {
        Err(e) => e,
        Ok(_) => panic!("{msg}"),
    }
}

/// T7b: 恢复入口校验 — 直接构造的检查点（绕过反序列化）也被拦截
#[tokio::test]
async fn t7b_restore_entry_validation_direct_construction() {
    use lellm_graph::{
        CheckpointConfig, GraphBuilder, InMemoryBlobStore, NodeKind, SerdeCheckpointCodec,
        SimpleExecutor, TaskNode, TraceId, TypedCheckpointStore,
    };
    use std::sync::Arc;

    let graph = {
        let mut b = GraphBuilder::<State>::new("ab");
        b.start("a");
        b.node("a", NodeKind::Task(TaskNode::new("a", |_ctx| Ok(()))));
        b.node("b", NodeKind::Task(TaskNode::new("b", |_ctx| Ok(()))));
        b.edge("a", "b");
        b.end("b");
        b.build().expect("build")
    };
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let config =
        || CheckpointConfig::for_store(store.clone(), SerdeCheckpointCodec::<State>::new(), hash);
    let tid = TraceId::new();
    let executor = SimpleExecutor::new(5);

    // 基准合法检查点（next=b, su=1）
    let base = Checkpoint::new(Some(NodeId("b".into())), &State::new(), hash, 1);

    // 1. format_version 错 → 拒绝
    let mut bad = base.clone();
    bad.format_version = 999;
    let err = expect_err(
        executor
            .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
            .await,
        "bad version",
    );
    assert!(err.to_string().contains("format_version"), "got: {err:?}");

    // 2. graph_hash 错 → 拒绝
    let mut bad = base.clone();
    bad.graph_hash = hash ^ 0xFF;
    let err = expect_err(
        executor
            .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
            .await,
        "bad hash",
    );
    assert!(
        err.to_string().contains("graph hash mismatch"),
        "got: {err:?}"
    );

    // 3. next_node 指向不存在节点 → 拒绝
    let mut bad = base.clone();
    bad.next_node = Some(NodeId("nonexistent".into()));
    let err = expect_err(
        executor
            .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
            .await,
        "bad node",
    );
    assert!(err.to_string().contains("nonexistent"), "got: {err:?}");

    // 4. next_node=Some 且 steps_used >= max_steps → 执行前报错
    let mut bad = base.clone();
    bad.steps_used = 5;
    let err = expect_err(
        executor
            .execute_stream_with_restore(Arc::new(graph.clone()), bad, tid, config())
            .await,
        "budget exhausted",
    );
    assert!(err.to_string().contains("step limit"), "got: {err:?}");

    // 5. next_node=None 且 steps_used >= max_steps → 合法（完成态，零执行）
    //    store=None 跳过最新性检查（续写目标由 save_fn 决定）
    let mut done = base.clone();
    done.next_node = None;
    done.steps_used = 5;
    let config_nostore = CheckpointConfig::new(
        move |_cp, _t| Box::pin(async { Ok::<(), CheckpointStoreError>(()) }),
        hash,
    );
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph.clone()), done, tid, config_nostore)
        .await
        .expect("completed state with exhausted budget is valid");
    let (_t, r) = drain_stream(exec.stream).await;
    r.expect("zero-execution complete");

    // 6. 非最新检查点 → RestoreNotLatest
    //    保存 cp1（next=b）与 cp2（next=None）到同一 trace，cp2 为最新
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&*store, codec);
    let cp1 = Checkpoint::new(Some(NodeId("b".into())), &State::new(), hash, 1);
    let cp2 = Checkpoint::new(None, &State::new(), hash, 2);
    typed
        .save_with_trace(&tid, &cp1, hash)
        .await
        .expect("save cp1");
    typed
        .save_with_trace(&tid, &cp2, hash)
        .await
        .expect("save cp2");
    let err = expect_err(
        executor
            .execute_stream_with_restore(Arc::new(graph), cp1, tid, config())
            .await,
        "not latest",
    );
    assert!(err.to_string().contains("not the latest"), "got: {err:?}");
}
