use async_trait::async_trait;
use lellm_graph::{durable::*, *};
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

struct Work {
    a: Arc<AtomicUsize>,
    b: Arc<AtomicUsize>,
    policy: ReplayPolicy,
    version: &'static str,
    keys: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl LeafNode<State> for Work {
    async fn execute(&self, ctx: &mut LeafContext<'_, State>) -> Result<(), GraphError> {
        let tasks = TaskRuntime::current().expect("durable context");
        let a: i64 = tasks
            .execute(
                "a",
                self.version,
                json!({"x": 1}),
                ReplayPolicy::ReplaySafe,
                |_| async {
                    self.a.fetch_add(1, Ordering::SeqCst);
                    Ok(10)
                },
            )
            .await?;
        let b: i64 = tasks
            .execute(
                "b",
                "v1",
                json!({"x": 2}),
                self.policy,
                |context| async move {
                    self.keys.lock().unwrap().push(context.idempotency_key);
                    if self.b.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(GraphError::Terminal(TerminalError::StateError(
                            "lost reply".into(),
                        )));
                    }
                    Ok(20)
                },
            )
            .await?;
        ctx.record(StateMutation::Put("sum".into(), json!(a + b)));
        Ok(())
    }
}
fn graph(work: Work) -> Graph {
    let mut builder = GraphBuilder::<State>::new("tasks");
    builder.start("work").end("work");
    builder.node("work", NodeKind::ExternalLeaf(Arc::new(work)));
    builder.build().unwrap()
}

#[tokio::test]
async fn completed_tasks_replay_and_idempotency_keys_survive_retry() {
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    let keys = Arc::new(Mutex::new(Vec::new()));
    let graph = graph(Work {
        a: a.clone(),
        b: b.clone(),
        policy: ReplayPolicy::IdempotentWithKey,
        version: "v1",
        keys: keys.clone(),
    });
    let runtime = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    );
    assert!(
        runtime
            .start("run", &graph, State::new(), 1, CancellationToken::new())
            .await
            .is_err()
    );
    let RunOutcome::Completed(state) = runtime
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(state.get_i64("sum"), Some(30));
    assert_eq!(a.load(Ordering::SeqCst), 1);
    assert_eq!(b.load(Ordering::SeqCst), 2);
    let keys = keys.lock().unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
    assert_eq!(runtime.inspect("run").unwrap().steps_used, 1);
}

#[tokio::test]
async fn unknown_result_requires_reconciliation_and_resolution_is_idempotent() {
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    let graph = graph(Work {
        a: a.clone(),
        b: b.clone(),
        policy: ReplayPolicy::ManualRecovery,
        version: "v1",
        keys: Default::default(),
    });
    let runtime = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    );
    assert!(matches!(
        runtime
            .start("run", &graph, State::new(), 1, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Suspended(_)
    ));
    assert!(matches!(
        runtime
            .resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Suspended(_)
    ));
    let record = runtime.inspect("run").unwrap();
    let task = record
        .tasks
        .values()
        .find(|t| t.status == TaskStatus::Unknown)
        .unwrap();
    for _ in 0..2 {
        runtime
            .resolve_task(
                "run",
                &task.id,
                "reconcile",
                record.revision,
                TaskResolution::Completed(json!(20)),
            )
            .unwrap();
    }
    assert!(
        runtime
            .resolve_task(
                "run",
                &task.id,
                "reconcile",
                record.revision,
                TaskResolution::Retry
            )
            .is_err()
    );
    let RunOutcome::Completed(state) = runtime
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(state.get_i64("sum"), Some(30));
    assert_eq!(a.load(Ordering::SeqCst), 1);
    assert_eq!(b.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn changing_task_version_refuses_cached_output() {
    let a = Arc::new(AtomicUsize::new(0));
    let b = Arc::new(AtomicUsize::new(0));
    let build = |version| {
        graph(Work {
            a: a.clone(),
            b: b.clone(),
            policy: ReplayPolicy::ReplaySafe,
            version,
            keys: Default::default(),
        })
    };
    let runtime = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    );
    assert!(
        runtime
            .start(
                "run",
                &build("v1"),
                State::new(),
                1,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(
        runtime
            .resume("run", &build("v2"), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(a.load(Ordering::SeqCst), 1);
    assert_eq!(b.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_last_node_cannot_complete_the_run() {
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let mut builder = GraphBuilder::<State>::new("cancel");
    builder.start("last").end("last");
    builder.node(
        "last",
        NodeKind::Task(TaskNode::new("last", move |_| {
            stop.cancel();
            Ok(())
        })),
    );
    let graph = builder.build().unwrap();
    let runtime = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    );
    assert!(matches!(
        runtime
            .start("run", &graph, State::new(), 1, cancel)
            .await
            .unwrap(),
        RunOutcome::Stopped(RunStatus::Cancelled)
    ));
    assert!(matches!(
        runtime
            .resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Stopped(RunStatus::Cancelled)
    ));
}

struct NullResult(Arc<AtomicUsize>, Arc<AtomicUsize>);
#[async_trait]
impl LeafNode<State> for NullResult {
    async fn execute(&self, _: &mut LeafContext<'_, State>) -> Result<(), GraphError> {
        TaskRuntime::current()
            .unwrap()
            .execute(
                "null",
                "v1",
                json!({}),
                ReplayPolicy::ManualRecovery,
                |_| async {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await?;
        if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(GraphError::Terminal(TerminalError::StateError(
                "after receipt".into(),
            )));
        }
        Ok(())
    }
}
#[tokio::test]
async fn saved_json_null_is_a_completed_result() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut b = GraphBuilder::<State>::new("null");
    b.start("task").end("task");
    b.node(
        "task",
        NodeKind::ExternalLeaf(Arc::new(NullResult(
            calls.clone(),
            Arc::new(AtomicUsize::new(0)),
        ))),
    );
    let graph = b.build().unwrap();
    let rt = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    );
    assert!(
        rt.start("run", &graph, State::new(), 1, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(matches!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Completed(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
