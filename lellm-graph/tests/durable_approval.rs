use lellm_graph::{durable::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

fn graph(calls: Arc<AtomicUsize>) -> Graph {
    let mut b = GraphBuilder::new("durable-approval");
    b.start("review").end("effect");
    b.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    b.node(
        "effect",
        NodeKind::Task(TaskNode::new("effect", move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })),
    );
    b.edge("review", "effect");
    b.build().unwrap()
}
fn runtime(store: Arc<dyn DurableStore>) -> DurableRuntime {
    DurableRuntime::new(store, RunVersion::new("approval-v1", "state-v1"))
}
fn approve(rt: &DurableRuntime, record: &RunRecord) -> DecisionReceipt {
    let request = record.approvals.values().next().unwrap();
    rt.submit_decision(
        &record.run_id,
        &request.id,
        &request.content_token,
        "decision-1",
        record.revision,
        ApprovalDecision::Approve,
    )
    .unwrap()
}

#[tokio::test]
async fn approval_survives_reopen_and_decision_retries() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = graph(calls.clone());
    let rt = runtime(Arc::new(FileRunStore::new(dir.path())));
    let RunOutcome::Suspended(record) = rt
        .start("run", &graph, State::new(), 2, CancellationToken::new())
        .await
        .unwrap()
    else {
        panic!("expected suspension")
    };
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(rt);
    let rt = runtime(Arc::new(FileRunStore::new(dir.path())));
    let first = approve(&rt, &record);
    assert_eq!(approve(&rt, &record), first); // 响应丢失后用原 revision 重试。
    let request = record.approvals.values().next().unwrap();
    assert!(
        rt.submit_decision(
            "run",
            &request.id,
            &request.content_token,
            "conflict",
            record.revision,
            ApprovalDecision::Reject {
                reason: "no".into()
            }
        )
        .is_err()
    );
    assert!(matches!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Completed(_)
    ));
    assert!(matches!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Completed(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(rt.inspect("run").unwrap().steps_used, 2);
}

#[tokio::test]
async fn reject_and_cancel_never_execute_following_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = graph(calls.clone());
    let rt = runtime(Arc::new(MemoryRunStore::default()));
    for id in ["reject", "cancel"] {
        rt.start(id, &graph, State::new(), 10, CancellationToken::new())
            .await
            .unwrap();
        let record = rt.inspect(id).unwrap();
        let req = record.approvals.values().next().unwrap();
        if id == "reject" {
            rt.submit_decision(
                id,
                &req.id,
                &req.content_token,
                "reject",
                record.revision,
                ApprovalDecision::Reject {
                    reason: "no".into(),
                },
            )
            .unwrap();
        } else {
            rt.cancel(id, record.revision).unwrap();
        }
        assert!(matches!(
            rt.resume(id, &graph, CancellationToken::new())
                .await
                .unwrap(),
            RunOutcome::Stopped(_)
        ));
        assert!(
            rt.submit_decision(
                id,
                &req.id,
                &req.content_token,
                "late",
                record.revision,
                ApprovalDecision::Approve
            )
            .is_err()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn stores_enforce_exclusive_ownership_and_release_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    for store in [
        Arc::new(MemoryRunStore::default()) as Arc<dyn DurableStore>,
        Arc::new(FileRunStore::new(dir.path())),
    ] {
        let lease = store.acquire("one").unwrap();
        assert!(matches!(store.acquire("one"), Err(DurableError::Busy)));
        assert!(store.acquire("two").is_ok());
        drop(lease);
        assert!(store.acquire("one").is_ok());
        assert!(store.acquire("../escape").is_err());
    }
}

#[tokio::test]
async fn incompatible_versions_and_missing_cursor_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FileRunStore::new(dir.path()));
    let rt = runtime(store.clone());
    let graph = graph(Arc::new(AtomicUsize::new(0)));
    rt.start("run", &graph, State::new(), 10, CancellationToken::new())
        .await
        .unwrap();
    let wrong = DurableRuntime::new(store, RunVersion::new("other", "state-v1"));
    assert!(
        wrong
            .resume("run", &graph, CancellationToken::new())
            .await
            .is_err()
    );
    let record = rt.inspect("run").unwrap();
    let request = record.approvals.values().next().unwrap();
    assert!(matches!(
        wrong.cancel("run", record.revision),
        Err(DurableError::Incompatible(_))
    ));
    assert!(matches!(
        wrong.submit_decision(
            "run",
            &request.id,
            &request.content_token,
            "wrong-version",
            record.revision,
            ApprovalDecision::Approve
        ),
        Err(DurableError::Incompatible(_))
    ));
    assert_eq!(rt.inspect("run").unwrap().revision, record.revision);
    let path = dir.path().join("run/run.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["frames"]["root"]
        .as_object_mut()
        .unwrap()
        .remove("next_node");
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(matches!(
        rt.inspect("run"),
        Err(DurableError::UnsupportedFormat)
    ));
}

#[tokio::test]
async fn expired_approval_does_not_use_legacy_default_approve() {
    let mut b = GraphBuilder::<State>::new("expired");
    b.start("review").end("review");
    b.node(
        "review",
        NodeKind::Barrier(BarrierNode::new("review").timeout(std::time::Duration::ZERO)),
    );
    let graph = b.build().unwrap();
    let rt = runtime(Arc::new(MemoryRunStore::default()));
    rt.start("run", &graph, State::new(), 1, CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        rt.resume("run", &graph, CancellationToken::new())
            .await
            .unwrap(),
        RunOutcome::Stopped(RunStatus::Expired)
    ));
}
