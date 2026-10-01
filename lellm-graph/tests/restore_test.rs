//! 恢复能力第一阶段 — 进程内执行级测试（T1/T2/T3/T6/T7）。
//!
//! 范围：串行含循环。承诺边界见 restore_new_process.rs 头部。
//!
//! 注意：T1/T2 依赖 Task 4/6 的 `CheckpointConfig::for_store` 与
//! `execute_stream_with_checkpoint` 入口 — Task 6 完成后本文件转绿。

use std::sync::Arc;

use lellm_graph::{
    BlobCheckpointStore, CheckpointCodec, CheckpointConfig, CheckpointStoreError, GraphBuilder,
    InMemoryBlobStore, SerdeCheckpointCodec, SimpleExecutor, State, TaskNode,
};

/// 线性图 a→b→c（end=c），每节点向 effects 追加自身名。
fn linear_graph(effects: &Arc<std::sync::Mutex<Vec<String>>>) -> lellm_graph::Graph {
    let g = effects.clone();
    let mk = |name: &str| {
        let g = g.clone();
        TaskNode::new(name, move |_ctx| {
            g.lock().expect("effects").push(name.to_string());
            Ok(())
        })
    };
    GraphBuilder::<State>::new("linear")
        .start("a")
        .node("a", mk("a"))
        .node("b", mk("b"))
        .node("c", mk("c"))
        .edge("a", "b")
        .edge("b", "c")
        .end("c")
        .build()
        .expect("build")
}

/// 消费事件流直至 GraphComplete/GraphError，返回 (trace_id, 结果)。
async fn drain(
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

fn config_for(store: Arc<InMemoryBlobStore>, hash: u64) -> CheckpointConfig<State> {
    CheckpointConfig::for_store(store, SerdeCheckpointCodec::<State>::new(), hash)
}

/// T1: 游标语义 — 每个成功提交节点保存「下一个节点」；end 节点后 next=None（完成态也保存）
#[tokio::test]
async fn t1_cursor_semantics_next_node_and_steps() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = linear_graph(&effects);
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let executor = SimpleExecutor::new(100);

    let exec = executor
        .execute_stream_with_checkpoint(
            Arc::new(graph),
            State::new(),
            config_for(store.clone(), hash),
        )
        .expect("entry");
    let (tid, result) = drain(exec.stream).await;
    result.expect("run should complete");

    // 3 个检查点：next=b(su=1) / next=c(su=2) / next=None(su=3)
    let ids = store.list(&tid).await.expect("list");
    assert_eq!(ids.len(), 3);
    // InMemory list 按插入倒序：[cp3, cp2, cp1]
    let codec = SerdeCheckpointCodec::<State>::new();
    let load = |id: &lellm_graph::CheckpointId| async {
        store
            .load(id)
            .await
            .expect("blob")
            .and_then(|b| codec.deserialize(&b, hash).ok())
            .expect("cp")
    };
    let cp3 = load(&ids[0]).await;
    let cp2 = load(&ids[1]).await;
    let cp1 = load(&ids[2]).await;
    assert_eq!(cp1.next_node, Some(lellm_graph::NodeId("b".into())));
    assert_eq!(cp1.steps_used, 1);
    assert_eq!(cp2.next_node, Some(lellm_graph::NodeId("c".into())));
    assert_eq!(cp2.steps_used, 2);
    assert_eq!(cp3.next_node, None, "completed state also saved");
    assert_eq!(cp3.steps_used, 3);
    assert_eq!(*effects.lock().expect("m"), vec!["a", "b", "c"]);
}

/// T2: 保存失败 → 执行在该边界停止，后续节点不执行
#[tokio::test]
async fn t2_save_failure_stops_at_boundary() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = linear_graph(&effects);
    let hash = graph.canonical_hash();

    // 永远失败的 save_fn
    let config = CheckpointConfig::new(
        move |_cp, _tid| Box::pin(async { Err(CheckpointStoreError::Storage("disk full".into())) }),
        hash,
    );

    let executor = SimpleExecutor::new(100);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config)
        .expect("entry");
    let (_tid, result) = drain(exec.stream).await;
    let err = result.expect_err("should fail at first checkpoint boundary");
    assert!(err.contains("checkpoint save failed"), "got: {err}");
    assert!(err.contains("disk full"), "got: {err}");
    // a 执行了（副作用已发生），b/c 未执行
    assert_eq!(*effects.lock().expect("m"), vec!["a"]);
}
