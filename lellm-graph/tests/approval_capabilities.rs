use lellm_graph::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

fn approval_graph(calls: Arc<AtomicUsize>) -> Graph {
    let mut b = GraphBuilder::new("approval");
    b.start("before").end("review");
    b.node(
        "before",
        NodeKind::Task(TaskNode::new("before", move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })),
    );
    b.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    b.edge("before", "review");
    b.build().unwrap()
}

#[tokio::test]
async fn missing_handler_fails_before_any_node_runs() {
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = approval_graph(calls.clone());
    let mut state = State::new();
    let mut engine = ExecutionEngine::new(&mut state, None, CancellationToken::new(), None, None);
    let error = graph
        .run_inline(&mut engine, 10, &mut NoopStepCallback)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        GraphError::Terminal(TerminalError::BarrierHandlerMissing { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn automatic_approval_requires_explicit_sink() {
    let calls = Arc::new(AtomicUsize::new(0));
    let graph = approval_graph(calls.clone());
    let mut state = State::new();
    let mut sink = NoopBarrierSink;
    let mut engine = ExecutionEngine::new(
        &mut state,
        None,
        CancellationToken::new(),
        None,
        Some(&mut sink),
    );
    graph
        .run_inline(&mut engine, 10, &mut NoopStepCallback)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn nested_approval_is_rejected_even_with_parent_handler() {
    let calls = Arc::new(AtomicUsize::new(0));
    let inner = Arc::new(approval_graph(calls.clone()));
    let mut b = GraphBuilder::<State>::new("outer");
    b.start("before").end("child");
    let outer_calls = calls.clone();
    b.node(
        "before",
        NodeKind::Task(TaskNode::new("before", move |_| {
            outer_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })),
    );
    b.subgraph("child", SubgraphSpec::new(inner, IdentityLens::new()));
    b.edge("before", "child");
    let graph = b.build().unwrap();
    let mut state = State::new();
    let mut sink = NoopBarrierSink;
    let mut engine = ExecutionEngine::new(
        &mut state,
        None,
        CancellationToken::new(),
        None,
        Some(&mut sink),
    );
    assert!(
        graph
            .run_inline(&mut engine, 10, &mut NoopStepCallback)
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
