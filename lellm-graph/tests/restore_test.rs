//! 恢复能力第一阶段 — 进程内执行级测试（T1/T2/T3/T6/T7）。
//!
//! 范围：串行含循环。承诺边界见 restore_new_process.rs 头部。
//!
//! 注意：T1/T2 依赖 Task 4/6 的 `CheckpointConfig::for_store` 与
//! `execute_stream_with_checkpoint` 入口 — Task 6 完成后本文件转绿。

use std::sync::Arc;

use lellm_graph::{
    BlobCheckpointStore, CheckpointCodec, CheckpointConfig, CheckpointStoreError, GraphBuilder,
    InMemoryBlobStore, NodeKind, SerdeCheckpointCodec, SimpleExecutor, State, TaskNode,
};

/// 线性图 a→b→c（end=c），每节点向 effects 追加自身名。
fn linear_graph(effects: &Arc<std::sync::Mutex<Vec<String>>>) -> lellm_graph::Graph {
    let g = effects.clone();
    let mk = |name: &str| {
        let g = g.clone();
        let name = name.to_string();
        TaskNode::new(name.clone(), move |_ctx| {
            g.lock().expect("effects").push(name.clone());
            Ok(())
        })
    };
    let mut b = GraphBuilder::<State>::new("linear");
    b.start("a");
    b.node("a", NodeKind::Task(mk("a")));
    b.node("b", NodeKind::Task(mk("b")));
    b.node("c", NodeKind::Task(mk("c")));
    b.edge("a", "b");
    b.edge("b", "c");
    b.end("c");
    b.build().expect("build")
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

/// expect_err 等价 — `GraphExecution` 未实现 Debug，标准 `expect_err` 不可用。
fn expect_err<T, E>(result: Result<T, E>, msg: &str) -> E {
    match result {
        Err(e) => e,
        Ok(_) => panic!("{msg}"),
    }
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
    let load = |id: &lellm_graph::CheckpointId| {
        let id = id.clone();
        let store = store.clone();
        let codec = codec.clone();
        async move {
            store
                .load(&id)
                .await
                .expect("blob")
                .and_then(|b| codec.deserialize(&b, hash).ok())
                .expect("cp")
        }
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

/// T3: 完成态恢复 — next=None → 零节点执行（副作用计数不变），返回完成态 state
#[tokio::test]
async fn t3_completed_restore_zero_execution() {
    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let graph = linear_graph(&effects);
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let executor = SimpleExecutor::new(100);

    // 首次跑完
    let exec = executor
        .execute_stream_with_checkpoint(
            Arc::new(graph.clone()),
            State::new(),
            config_for(store.clone(), hash),
        )
        .expect("entry");
    let (tid, result) = drain(exec.stream).await;
    result.expect("first run");
    assert_eq!(*effects.lock().expect("m"), vec!["a", "b", "c"]);

    // 加载完成态检查点（next=None）
    let codec = SerdeCheckpointCodec::<State>::new();
    let latest = store
        .load_latest(&tid)
        .await
        .expect("latest")
        .expect("some");
    let cp = codec.deserialize(&latest, hash).expect("cp");
    assert_eq!(cp.next_node, None);

    // 恢复 → 零执行
    let effects_before = effects.lock().expect("m").len();
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, tid, config_for(store, hash))
        .await
        .expect("restore entry");
    let (_tid2, result) = drain(exec.stream).await;
    result.expect("completed restore");
    assert_eq!(
        effects.lock().expect("m").len(),
        effects_before,
        "zero node execution"
    );
}

/// T6: 持久化入口拒绝 Parallel/Subgraph/Barrier；非持久化不受影响
#[tokio::test]
async fn t6_persistence_entry_rejects_unsupported_graphs() {
    use lellm_graph::{
        BarrierNode, CompiledSubgraph, FlowNode, IdentityLens, ParallelNode, SubgraphSpec,
    };

    // Parallel 图
    let parallel_graph = {
        let mut b = GraphBuilder::<State>::new("par");
        b.start("p");
        b.node(
            "p",
            NodeKind::Parallel(
                ParallelNode::builder()
                    .branch(
                        "x",
                        Arc::new(TaskNode::new("x", |_ctx| Ok(()))) as Arc<dyn FlowNode<State>>,
                    )
                    .build(),
            ),
        );
        b.end("p");
        b.build().expect("build")
    };
    let hash = parallel_graph.canonical_hash();
    let executor = SimpleExecutor::new(100);
    let err = expect_err(
        executor.execute_stream_with_checkpoint(
            Arc::new(parallel_graph.clone()),
            State::new(),
            config_for(Arc::new(InMemoryBlobStore::new()), hash),
        ),
        "must reject at entry",
    );
    let msg = err.to_string();
    assert!(msg.contains("Parallel"), "got: {msg}");
    assert!(
        msg.contains("RestoreUnsupported") || msg.contains("does not support"),
        "got: {msg}"
    );

    // 同一 Parallel 图非持久化执行 → 正常（原行为）
    let exec = executor.execute_stream(Arc::new(parallel_graph), State::new());
    let (_t, r) = drain(exec.stream).await;
    r.expect("non-persistent execution unaffected");

    // Barrier 图
    let barrier_graph = {
        let mut b = GraphBuilder::<State>::new("bar");
        b.start("h");
        b.node("h", NodeKind::Barrier(BarrierNode::new("human")));
        b.end("h");
        b.build().expect("build")
    };
    let err = expect_err(
        executor.execute_stream_with_checkpoint(
            Arc::new(barrier_graph),
            State::new(),
            config_for(Arc::new(InMemoryBlobStore::new()), 0),
        ),
        "must reject barrier",
    );
    assert!(err.to_string().contains("Barrier"));

    // Subgraph 图
    let inner = {
        let mut b = GraphBuilder::<State>::new("inner");
        b.start("i");
        b.node("i", NodeKind::Task(TaskNode::new("i", |_ctx| Ok(()))));
        b.end("i");
        b.build().expect("build")
    };
    let sub_graph = {
        let mut b = GraphBuilder::<State>::new("sub");
        b.start("s");
        b.node(
            "s",
            NodeKind::Subgraph(CompiledSubgraph::new(
                Arc::new(SubgraphSpec::new(
                    Arc::new(inner),
                    IdentityLens::<State>::new(),
                )),
                100,
            )),
        );
        b.end("s");
        b.build().expect("build")
    };
    let err = expect_err(
        executor.execute_stream_with_checkpoint(
            Arc::new(sub_graph),
            State::new(),
            config_for(Arc::new(InMemoryBlobStore::new()), 0),
        ),
        "must reject subgraph",
    );
    assert!(err.to_string().contains("Subgraph"));
}

/// 循环图 init→check⇄work→done（count<3 时 check→work，否则 check→done）。
fn loop_graph(
    effects: &Arc<std::sync::Mutex<Vec<String>>>,
    work_fail: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> lellm_graph::Graph {
    use lellm_graph::StateExt;

    let g = effects.clone();
    let append = |name: &str| {
        let g = g.clone();
        let name = name.to_string();
        move |_ctx: &mut lellm_graph::NodeContext| {
            g.lock().expect("effects").push(name.clone());
            Ok(())
        }
    };
    let mut b = GraphBuilder::<State>::new("loop");
    b.start("init");
    b.node(
        "init",
        NodeKind::Task(TaskNode::new("init", append("init"))),
    );

    let check_fn = append("check");
    b.node("check", NodeKind::Task(TaskNode::new("check", check_fn)));

    let work_fn = {
        let g = g.clone();
        let fail = work_fail.clone();
        move |ctx: &mut lellm_graph::NodeContext| {
            if let Some(f) = &fail {
                if f.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(lellm_graph::GraphError::Terminal(
                        lellm_graph::TerminalError::StateError("simulated crash".into()),
                    ));
                }
            }
            g.lock().expect("effects").push("work".to_string());
            let count = ctx.state().get_i64("count").unwrap_or(0);
            ctx.record(lellm_graph::StateMutation::Put(
                "count".into(),
                serde_json::json!(count + 1),
            ));
            Ok(())
        }
    };
    b.node("work", NodeKind::Task(TaskNode::new("work", work_fn)));
    b.node(
        "done",
        NodeKind::Task(TaskNode::new("done", append("done"))),
    );

    b.edge("init", "check");
    b.edge_if("check", "work", |s: &State| {
        s.get_i64("count").unwrap_or(0) < 3
    });
    b.edge_if("check", "done", |s: &State| {
        s.get_i64("count").unwrap_or(0) >= 3
    });
    b.edge("work", "check");
    b.end("done");
    b.build().expect("build")
}

/// T7: 步数预算跨恢复延续 — 崩溃后恢复不重新获得完整 max_steps；到限后下一节点未执行
#[tokio::test]
async fn t7_loop_budget_continues_across_restore() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let effects = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fail = Arc::new(AtomicBool::new(true)); // work 首次执行即「崩溃」
    let graph = loop_graph(&effects, Some(fail.clone()));
    let hash = graph.canonical_hash();
    let store = Arc::new(InMemoryBlobStore::new());
    let executor = SimpleExecutor::new(5); // 总预算 5

    // 阶段 A：init(1) check(2) work(3)→Err 停止。最新检查点 = next=work, su=2
    let exec = executor
        .execute_stream_with_checkpoint(
            Arc::new(graph.clone()),
            State::new(),
            config_for(store.clone(), hash),
        )
        .expect("entry");
    let (tid, result) = drain(exec.stream).await;
    let err = result.expect_err("stage A crashes at work");
    assert!(err.contains("simulated crash"), "got: {err}");

    let codec = SerdeCheckpointCodec::<State>::new();
    let latest = store
        .load_latest(&tid)
        .await
        .expect("latest")
        .expect("some");
    let cp = codec.deserialize(&latest, hash).expect("cp");
    assert_eq!(cp.next_node, Some(lellm_graph::NodeId("work".into())));
    assert_eq!(cp.steps_used, 2);

    // 阶段 B：恢复（同一总预算 5）→ work(3) check(4) work(5) → step6 StepsExceeded
    fail.store(false, Ordering::SeqCst);
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, tid, config_for(store, hash))
        .await
        .expect("restore entry");
    let (_tid2, result) = drain(exec.stream).await;
    let err = result.expect_err("stage B hits total budget");
    assert!(err.contains("step limit 5 exceeded"), "got: {err}");

    // 总执行 = 5 步：init check work | check work — 第 6 步（check）未执行
    assert_eq!(
        *effects.lock().expect("m"),
        vec!["init", "check", "work", "check", "work"]
    );
}
