//! 恢复能力第一阶段 — 存储与保留策略测试（T5 / T7c）。
//!
//! 注意：T7c 依赖 Task 6 的 `execute_stream_with_checkpoint` 入口 — Task 6 后转绿。

use std::sync::Arc;

use lellm_graph::{
    CheckpointConfig, CheckpointStoreError, GraphBuilder, InMemoryBlobStore, NodeKind,
    RetentionPolicy, SerdeCheckpointCodec, SimpleExecutor, State, TaskNode,
};

fn two_node_graph(effects: &Arc<std::sync::Mutex<Vec<String>>>) -> lellm_graph::Graph {
    let g = effects.clone();
    let mk = |name: &str| {
        let g = g.clone();
        TaskNode::new(name, move |_ctx| {
            g.lock().expect("effects").push(name.to_string());
            Ok(())
        })
    };
    let mut b = GraphBuilder::<State>::new("two");
    b.start("a");
    b.node("a", NodeKind::Task(mk("a")));
    b.node("b", NodeKind::Task(mk("b")));
    b.edge("a", "b");
    b.end("b");
    b.build().expect("build")
}

/// T7c: KeepLatest(0) → 保存路径明确报错（避免保存后删掉全部恢复点）
#[tokio::test]
async fn t7c_keep_latest_zero_rejected() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = two_node_graph(&effects);
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());

    let config = CheckpointConfig::for_store(store, SerdeCheckpointCodec::<State>::new(), hash)
        .with_retention(RetentionPolicy::KeepLatest(0));

    let executor = SimpleExecutor::new(100);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config)
        .expect("entry");

    // 消费至终止事件
    let mut got_error = None;
    while let Some(ev) = exec.stream.recv().await {
        if let lellm_graph::GraphEvent::GraphError { error, .. } = ev {
            got_error = Some(error.to_string());
            break;
        }
    }
    let err = got_error.expect("should fail on KeepLatest(0)");
    assert!(err.contains("KeepLatest(0)"), "got: {err}");
    // a 执行了（副作用发生），b 未执行
    assert_eq!(*effects.lock().expect("m"), vec!["a"]);
}

/// T5: FileBlobStore — 往返 / seq 单调（含重建延续）/ .tmp 排除 / 最新损坏不回退 / seq 溢出
#[tokio::test]
async fn t5_file_store_roundtrip_seq_tmp_corruption_overflow() {
    use lellm_graph::{BlobCheckpointStore, CheckpointBlob, CheckpointId, FileBlobStore, TraceId};
    use std::time::SystemTime;

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    let trace = TraceId::new();
    let hash = 0xABCD_u64;

    let make_blob = |tag: u8| {
        CheckpointBlob::new(
            CheckpointId::new(),
            vec![tag, tag, tag],
            hash,
            SystemTime::now(),
        )
    };

    // 1. 保存 3 个 → load 逐个往返 → load_latest = 第 3 个 → list 倒序 3 个
    let store = FileBlobStore::new(root.clone());
    let (id1, id2, id3) = (make_blob(1), make_blob(2), make_blob(3));
    store.save_with_trace(&trace, &id1).await.expect("save1");
    store.save_with_trace(&trace, &id2).await.expect("save2");
    store.save_with_trace(&trace, &id3).await.expect("save3");

    assert_eq!(
        store.load(&id1.id).await.expect("l1").unwrap().data,
        vec![1, 1, 1]
    );
    let latest = store
        .load_latest(&trace)
        .await
        .expect("latest")
        .expect("some");
    assert_eq!(latest.id, id3.id, "latest = 3rd saved");
    let ids = store.list(&trace).await.expect("list");
    assert_eq!(ids, vec![id3.id, id2.id, id1.id], "list by seq desc");

    // 2. seq 延续：重建 store（模拟新进程），保存 → seq 继续（不重置）
    drop(store);
    let store2 = FileBlobStore::new(root.clone());
    let id4 = make_blob(4);
    store2.save_with_trace(&trace, &id4).await.expect("save4");
    let latest = store2.load_latest(&trace).await.expect("latest2").unwrap();
    assert_eq!(latest.id, id4.id, "seq continues after store rebuild");

    // 3. .tmp 残留不参与 latest / list
    let trace_dir = root.join(trace.to_string());
    let tmp_name = format!("99_{}.tmp", CheckpointId::new());
    std::fs::write(trace_dir.join(&tmp_name), b"partial write").expect("tmp file");
    let latest = store2.load_latest(&trace).await.expect("latest3").unwrap();
    assert_eq!(latest.id, id4.id, ".tmp residue ignored by load_latest");
    let ids = store2.list(&trace).await.expect("list3");
    assert_eq!(ids.len(), 4, ".tmp not listed");

    // 4. prune(2) → 保留 seq 最大的 2 个
    let pruned = store2.prune(&trace, 2).await.expect("prune");
    assert_eq!(pruned, 2);
    let ids = store2.list(&trace).await.expect("list4");
    assert_eq!(ids, vec![id4.id, id3.id], "prune keeps newest by seq");

    // 5. 最新文件损坏 → load_latest 报 Corrupted（不回退旧检查点）
    let latest_path = trace_dir
        .read_dir()
        .expect("dir")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains(&id4.id.to_string()))
        .expect("latest file");
    std::fs::write(&latest_path, b"corrupted!!").expect("corrupt");
    match store2.load_latest(&trace).await {
        Err(CheckpointStoreError::Corrupted(_)) => {}
        other => panic!("expected Corrupted (no fallback), got: {other:?}"),
    }

    // 6. seq 溢出：制造 u64::MAX seq 文件 → 保存报错
    let overflow_trace = TraceId::new();
    let overflow_dir = root.join(overflow_trace.to_string());
    std::fs::create_dir_all(&overflow_dir).expect("dir");
    let overflow_id = CheckpointId::new();
    std::fs::write(
        overflow_dir.join(format!("{}_{overflow_id}", u64::MAX)),
        b"{}",
    )
    .expect("overflow file");
    let store3 = FileBlobStore::new(root.clone());
    match store3.save_with_trace(&overflow_trace, &make_blob(9)).await {
        Err(CheckpointStoreError::Storage(msg)) => {
            assert!(msg.contains("seq overflow"), "got: {msg}")
        }
        other => panic!("expected seq overflow Storage error, got: {other:?}"),
    }
}
