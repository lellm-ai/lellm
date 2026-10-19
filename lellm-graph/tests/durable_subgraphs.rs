use lellm_graph::{durable::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn child(calls: Arc<AtomicUsize>) -> Arc<Graph> {
    let mut b = GraphBuilder::new("child");
    b.start("review").end("effect");
    b.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    b.node(
        "effect",
        NodeKind::Task(TaskNode::new("effect", move |ctx| {
            calls.fetch_add(1, Ordering::SeqCst);
            ctx.record(StateMutation::Put("answer".into(), serde_json::json!(42)));
            Ok(())
        })),
    );
    b.edge("review", "effect");
    Arc::new(b.build().unwrap())
}
fn wrap(inner: Arc<Graph>, name: &str) -> Arc<Graph> {
    let mut b = GraphBuilder::<State>::new(name);
    b.start("child").end("child");
    b.subgraph("child", SubgraphSpec::new(inner, IdentityLens::new()));
    Arc::new(b.build().unwrap())
}

#[tokio::test]
async fn nested_approval_has_stable_path_and_shared_budget() {
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = wrap(wrap(child(calls.clone()), "middle"), "root");
    let rt = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    );
    let RunOutcome::Suspended(record) = rt
        .start("run", &graph, State::new(), 4, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    let request = record.approvals.values().next().unwrap();
    assert_ne!(request.frame, "root");
    assert_eq!(record.frames.len(), 3);
    assert_eq!(record.steps_used, 3);
    assert!(matches!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Suspended(_)
    ));
    rt.submit_decision(
        "run",
        &request.id,
        &request.content_token,
        "approve",
        record.revision,
        ApprovalDecision::Approve,
    )
    .unwrap();
    let RunOutcome::Completed(state) = rt
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(state.get_i64("answer"), Some(42));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(rt.inspect("run").unwrap().steps_used, 4);
}

struct FailParentStore {
    inner: MemoryRunStore,
    fail: Arc<AtomicBool>,
}
struct FailParentLease {
    inner: Box<dyn RunLease>,
    fail: Arc<AtomicBool>,
}
impl DurableStore for FailParentStore {
    fn acquire(&self, id: &str) -> Result<Box<dyn RunLease>, DurableError> {
        Ok(Box::new(FailParentLease {
            inner: self.inner.acquire(id)?,
            fail: self.fail.clone(),
        }))
    }
    fn inspect(&self, id: &str) -> Result<RunRecord, DurableError> {
        self.inner.inspect(id)
    }
}
impl RunLease for FailParentLease {
    fn load(&self) -> Result<Option<RunRecord>, DurableError> {
        self.inner.load()
    }
    fn save(&self, r: &RunRecord) -> Result<(), DurableError> {
        if r.frames["root"].next_node.is_none() && self.fail.load(Ordering::SeqCst) {
            return Err(DurableError::Io(std::io::Error::other(
                "parent commit failed",
            )));
        }
        self.inner.save(r)
    }
}

#[tokio::test]
async fn child_completion_survives_parent_commit_failure() {
    let fail = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = wrap(child(calls.clone()), "root");
    let rt = DurableRuntime::new(
        Arc::new(FailParentStore {
            inner: Default::default(),
            fail: fail.clone(),
        }),
        RunVersion::new("v1", "v1"),
    );
    let RunOutcome::Suspended(record) = rt
        .start("run", &graph, State::new(), 3, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    let req = record.approvals.values().next().unwrap();
    rt.submit_decision(
        "run",
        &req.id,
        &req.content_token,
        "approve",
        record.revision,
        ApprovalDecision::Approve,
    )
    .unwrap();
    assert!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    fail.store(false, Ordering::SeqCst);
    let RunOutcome::Completed(state) = rt
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(state.get_i64("answer"), Some(42));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn parallel_results_survive_parent_merge_commit_failure() {
    let fail = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut parallel = ParallelNode::builder();
    for name in ["a", "b"] {
        let calls = calls.clone();
        parallel = parallel.branch(
            name,
            Arc::new(TaskNode::new(name, move |ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                ctx.record(StateMutation::Put(name.into(), serde_json::json!(1)));
                Ok(())
            })),
        );
    }
    let mut b = GraphBuilder::new("parallel-parent");
    b.start("parallel").end("parallel");
    b.node("parallel", NodeKind::Parallel(parallel.build()));
    let graph = b.build().unwrap();
    let rt = DurableRuntime::new(
        Arc::new(FailParentStore {
            inner: Default::default(),
            fail: fail.clone(),
        }),
        RunVersion::new("v1", "v1"),
    );
    assert!(
        rt.start("run", &graph, State::new(), 3, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    fail.store(false, Ordering::SeqCst);
    let RunOutcome::Completed(state) = rt
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(state.get_i64("a"), Some(1));
    assert_eq!(state.get_i64("b"), Some(1));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(matches!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Completed(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn nested_events_include_the_persisted_frame_path() {
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = events.clone();
    let graph = wrap(child(Arc::new(AtomicUsize::new(0))), "root");
    let rt = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    )
    .with_observer(Arc::new(move |event| seen.lock().unwrap().push(event)));
    let RunOutcome::Suspended(record) = rt
        .start("run", &graph, State::new(), 3, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    let events = events.lock().unwrap();
    let event = events
        .iter()
        .find(|e| e.phase == "waiting_approval")
        .unwrap();
    assert_eq!(event.frame, record.approvals.values().next().unwrap().frame);
    assert_eq!(event.node, "review");
    assert!(event.revision <= record.revision);
}
