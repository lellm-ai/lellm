//! 恢复能力第一阶段 — 存储与保留策略测试（T5 / T7c）。
//!
//! 注意：T7c 依赖 Task 6 的 `execute_stream_with_checkpoint` 入口 — Task 6 后转绿。

use std::sync::Arc;

use lellm_graph::{
    CheckpointConfig, GraphBuilder, InMemoryBlobStore, RetentionPolicy, SerdeCheckpointCodec,
    SimpleExecutor, State, TaskNode,
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
    GraphBuilder::<State>::new("two")
        .start("a")
        .node("a", mk("a"))
        .node("b", mk("b"))
        .edge("a", "b")
        .end("b")
        .build()
        .expect("build")
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
