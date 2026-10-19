use lellm_graph::{durable::*, *};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct FaultStore {
    inner: MemoryRunStore,
    mode: Arc<AtomicUsize>,
}
struct FaultLease {
    inner: Box<dyn RunLease>,
    mode: Arc<AtomicUsize>,
}
impl DurableStore for FaultStore {
    fn acquire(&self, id: &str) -> Result<Box<dyn RunLease>, DurableError> {
        Ok(Box::new(FaultLease {
            inner: self.inner.acquire(id)?,
            mode: self.mode.clone(),
        }))
    }
    fn inspect(&self, id: &str) -> Result<RunRecord, DurableError> {
        self.inner.inspect(id)
    }
}
impl RunLease for FaultLease {
    fn load(&self) -> Result<Option<RunRecord>, DurableError> {
        self.inner.load()
    }
    fn save(&self, r: &RunRecord) -> Result<(), DurableError> {
        let fail = match self.mode.load(Ordering::SeqCst) {
            1 => !r.approvals.is_empty(),
            2 => r.approvals.values().any(|a| a.receipt.is_some()),
            3 => r.approvals.values().any(|a| a.applied),
            _ => false,
        };
        if fail {
            return Err(DurableError::Io(std::io::Error::other(
                "injected save failure",
            )));
        }
        self.inner.save(r)
    }
}

#[tokio::test]
async fn saving_wait_decision_or_applied_cursor_failure_blocks_successor() {
    for fail_at in [1, 2, 3] {
        let mode = Arc::new(AtomicUsize::new(if fail_at == 1 { 1 } else { 0 }));
        let rt = DurableRuntime::new(
            Arc::new(FaultStore {
                inner: MemoryRunStore::default(),
                mode: mode.clone(),
            }),
            RunVersion::new("v1", "v1"),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let effect_calls = calls.clone();
        let mut b = GraphBuilder::<State>::new("fault");
        b.start("review").end("effect");
        b.node("review", NodeKind::Barrier(BarrierNode::new("review")));
        b.node(
            "effect",
            NodeKind::Task(TaskNode::new("effect", move |_| {
                effect_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
        );
        b.edge("review", "effect");
        let graph = b.build().unwrap();
        let start = rt
            .start("run", &graph, State::new(), 2, CancellationToken::new())
            .await;
        if fail_at == 1 {
            assert!(start.is_err());
            mode.store(0, Ordering::SeqCst);
            assert!(matches!(
                rt.resume("run", &graph, CancellationToken::new())
                    .await
                    .unwrap(),
                RunOutcome::Suspended(_)
            ));
        }
        let record = rt.inspect("run").unwrap();
        let request = record.approvals.values().next().unwrap();
        if fail_at == 2 {
            mode.store(2, Ordering::SeqCst);
        }
        let submit = || {
            rt.submit_decision(
                "run",
                &request.id,
                &request.content_token,
                "one",
                record.revision,
                ApprovalDecision::Approve,
            )
        };
        if fail_at == 2 {
            assert!(submit().is_err());
            assert!(
                rt.inspect("run")
                    .unwrap()
                    .approvals
                    .values()
                    .all(|a| a.receipt.is_none())
            );
            mode.store(0, Ordering::SeqCst);
        }
        submit().unwrap();
        if fail_at == 3 {
            mode.store(3, Ordering::SeqCst);
            assert!(
                rt.resume("run", &graph, CancellationToken::new())
                    .await
                    .is_err()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            mode.store(0, Ordering::SeqCst);
        }
        assert!(matches!(
            rt.resume("run", &graph, CancellationToken::new())
                .await
                .unwrap(),
            RunOutcome::Completed(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
