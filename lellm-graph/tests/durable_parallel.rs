#![allow(deprecated)]
use async_trait::async_trait;
use lellm_graph::{durable::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;

fn graph(parallel: ParallelNode) -> Graph {
    let mut b = GraphBuilder::<State>::new("parallel");
    b.start("parallel").end("parallel");
    b.node("parallel", NodeKind::Parallel(parallel));
    b.build().unwrap()
}
fn runtime() -> DurableRuntime {
    DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("v1", "v1"),
    )
}

#[tokio::test]
async fn successful_branch_is_reused_after_sibling_failure() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let fail = Arc::new(AtomicBool::new(true));
    let fails = fail.clone();
    let parallel = ParallelNode::builder()
        .branch(
            "a",
            Arc::new(TaskNode::<State>::new("a", move |ctx| {
                count.fetch_add(1, Ordering::SeqCst);
                ctx.record(StateMutation::Put("a".into(), serde_json::json!(1)));
                Ok(())
            })),
        )
        .branch(
            "b",
            Arc::new(TaskNode::<State>::new("b", move |ctx| {
                if fails.load(Ordering::SeqCst) {
                    return Err(GraphError::Terminal(TerminalError::StateError(
                        "b failed".into(),
                    )));
                }
                ctx.record(StateMutation::Put("b".into(), serde_json::json!(2)));
                Ok(())
            })),
        )
        .error_strategy(ParallelErrorStrategy::CollectAll)
        .build();
    let graph = graph(parallel);
    let rt = runtime();
    assert!(
        rt.start("run", &graph, State::new(), 3, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        rt.inspect("run").unwrap().frames["root"]
            .state
            .get("a")
            .is_none()
    );
    fail.store(false, Ordering::SeqCst);
    let RunOutcome::Completed(state) = rt
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.get_i64("a"), Some(1));
    assert_eq!(state.get_i64("b"), Some(2));
    assert_eq!(rt.inspect("run").unwrap().steps_used, 3);
}

struct WaitForCancel {
    started: Arc<Notify>,
    cancelled: Arc<AtomicBool>,
}
#[async_trait]
impl FlowNode for WaitForCancel {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        self.started.notify_one();
        ctx.cancel_token().cancelled().await;
        self.cancelled.store(true, Ordering::SeqCst);
        Err(GraphError::Terminal(TerminalError::StateError(
            "sibling cancelled".into(),
        )))
    }
}
struct FailWhenStarted(Arc<Notify>);
#[async_trait]
impl FlowNode for FailWhenStarted {
    async fn execute(&self, _: &mut NodeContext<'_>) -> Result<(), GraphError> {
        self.0.notified().await;
        Err(GraphError::Terminal(TerminalError::StateError(
            "original failure".into(),
        )))
    }
}

#[tokio::test]
async fn fail_fast_requests_cancellation_before_waiting_for_siblings() {
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let graph = graph(
        ParallelNode::builder()
            .branch(
                "waiting",
                Arc::new(WaitForCancel {
                    started: started.clone(),
                    cancelled: cancelled.clone(),
                }),
            )
            .branch("failed", Arc::new(FailWhenStarted(started)))
            .build(),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        runtime().start("run", &graph, State::new(), 3, CancellationToken::new()),
    )
    .await
    .expect("FailFast must cancel waiting sibling");
    assert!(result.unwrap_err().to_string().contains("original failure"));
    assert!(cancelled.load(Ordering::SeqCst));
}

fn review_graph(key: &'static str, calls: Arc<AtomicUsize>) -> Arc<Graph> {
    let mut b = GraphBuilder::<State>::new(key);
    b.start("review").end("effect");
    b.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    b.node(
        "effect",
        NodeKind::Task(TaskNode::<State>::new("effect", move |ctx| {
            calls.fetch_add(1, Ordering::SeqCst);
            ctx.record(StateMutation::Put(key.into(), serde_json::json!(1)));
            Ok(())
        })),
    );
    b.edge("review", "effect");
    Arc::new(b.build().unwrap())
}

#[tokio::test]
async fn parallel_graph_approvals_require_all_decisions_and_keep_separate_namespaces() {
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = graph(
        ParallelNode::builder()
            .graph_branch("a", review_graph("a", calls.clone()), 2)
            .graph_branch("b", review_graph("b", calls.clone()), 2)
            .build(),
    );
    let rt = runtime();
    let RunOutcome::Suspended(record) = rt
        .start("run", &graph, State::new(), 7, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(record.approvals.len(), 2);
    assert_eq!(record.frames.len(), 5);
    for (i, req) in record.approvals.values().enumerate() {
        let revision = rt.inspect("run").unwrap().revision;
        rt.submit_decision(
            "run",
            &req.id,
            &req.content_token,
            &format!("approval-{i}"),
            revision,
            ApprovalDecision::Approve,
        )
        .unwrap();
        if i == 0 {
            assert!(matches!(
                rt.resume("run", &graph, CancellationToken::new())
                    .await
                    .unwrap(),
                RunOutcome::Suspended(_)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
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
    assert_eq!(rt.inspect("run").unwrap().steps_used, 7);
}

#[tokio::test]
async fn collect_all_reports_every_failure() {
    let fail = |name: &'static str| {
        Arc::new(TaskNode::<State>::new(name, move |_| {
            Err(GraphError::Terminal(TerminalError::StateError(name.into())))
        }))
    };
    let graph = graph(
        ParallelNode::builder()
            .branch("a", fail("a"))
            .branch("b", fail("b"))
            .error_strategy(ParallelErrorStrategy::CollectAll)
            .build(),
    );
    let error = runtime()
        .start("run", &graph, State::new(), 3, CancellationToken::new())
        .await
        .unwrap_err();
    let GraphError::Terminal(TerminalError::ParallelFailures { errors }) = error else {
        panic!("expected aggregated errors")
    };
    assert_eq!(errors.len(), 2);
}

#[tokio::test]
async fn loop_reentry_allocates_new_branch_and_approval_identity() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut child = GraphBuilder::<State>::new("loop-child");
    child.start("review").end("work");
    child.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    child.node(
        "work",
        NodeKind::Task(TaskNode::<State>::new("work", move |ctx| {
            count.fetch_add(1, Ordering::SeqCst);
            ctx.record(StateMutation::Put(
                "round".into(),
                serde_json::json!(ctx.state().get_i64("round").unwrap_or(0) + 1),
            ));
            Ok(())
        })),
    );
    child.edge("review", "work");
    let mut b = GraphBuilder::<State>::new("loop");
    b.start("parallel").end("route");
    b.node(
        "parallel",
        NodeKind::Parallel(
            ParallelNode::builder()
                .graph_branch("a", Arc::new(child.build().unwrap()), 2)
                .build(),
        ),
    );
    b.node(
        "route",
        NodeKind::Task(TaskNode::<State>::new("route", |ctx| {
            if ctx.state().get_i64("round").unwrap_or(0) < 2 {
                ctx.goto("parallel");
            }
            Ok(())
        })),
    );
    b.edge("parallel", "route");
    let graph = b.build().unwrap();
    let rt = runtime();
    let RunOutcome::Suspended(first) = rt
        .start("run", &graph, State::new(), 10, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    let req = first.approvals.values().next().unwrap();
    rt.submit_decision(
        "run",
        &req.id,
        &req.content_token,
        "first",
        first.revision,
        ApprovalDecision::Approve,
    )
    .unwrap();
    let RunOutcome::Suspended(second) = rt
        .resume("run", &graph, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!()
    };
    let next = second
        .approvals
        .values()
        .find(|r| r.receipt.is_none())
        .unwrap();
    assert_ne!(req.id, next.id);
    assert_ne!(req.frame, next.frame);
    rt.submit_decision(
        "run",
        &next.id,
        &next.content_token,
        "second",
        second.revision,
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
    assert_eq!(state.get_i64("round"), Some(2));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(rt.inspect("run").unwrap().steps_used, 10);
}
