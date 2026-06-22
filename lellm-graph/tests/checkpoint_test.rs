//! Checkpoint 集成测试。
//!
//! 使用 NodeContext API（v04+）。

use lellm_graph::{
    BarrierDecision, CheckpointPolicy, CheckpointStore, GraphBuilder, GraphExecutor,
    InMemoryCheckpointStore, NodeContext, NodeKind, State, StateEffect, TaskNode,
};
use std::sync::Arc;

/// 构建一个简单的 3 节点线性图：a → b → c
fn build_simple_graph() -> Arc<lellm_graph::Graph> {
    let mut g = GraphBuilder::new("test");
    g.start("a").end("c");
    g.node(
        "a",
        NodeKind::Task(TaskNode::new("a", |ctx: &mut NodeContext<'_>| {
            ctx.emit_effect(StateEffect::Put("step".into(), serde_json::json!(1u32)));
            Ok(())
        })),
    );
    g.node(
        "b",
        NodeKind::Task(TaskNode::new("b", |ctx: &mut NodeContext<'_>| {
            ctx.emit_effect(StateEffect::Put("step".into(), serde_json::json!(2u32)));
            ctx.emit_effect(StateEffect::Put("done".into(), serde_json::json!(true)));
            Ok(())
        })),
    );
    g.node(
        "c",
        NodeKind::Task(TaskNode::new("c", |ctx: &mut NodeContext<'_>| {
            ctx.emit_effect(StateEffect::Put("final".into(), serde_json::json!(true)));
            Ok(())
        })),
    );
    g.edge("a", "b");
    g.edge("b", "c");
    Arc::new(g.build().expect("build"))
}

// ─── Graph::hash() 测试 ────────────────────────────────────

#[test]
fn test_graph_hash_deterministic() {
    let graph = build_simple_graph();
    assert_eq!(graph.hash(), graph.hash());
    assert_eq!(graph.hash().len(), 16);
}

#[test]
fn test_graph_hash_differs_on_structure_change() {
    let graph1 = build_simple_graph();
    let mut g2 = GraphBuilder::new("diff");
    g2.start("a").end("c");
    g2.node(
        "a",
        NodeKind::Task(TaskNode::new("a", |_ctx: &mut NodeContext<'_>| Ok(()))),
    );
    g2.node(
        "c",
        NodeKind::Task(TaskNode::new("c", |_ctx: &mut NodeContext<'_>| Ok(()))),
    );
    g2.edge("a", "c");
    let graph2 = Arc::new(g2.build().unwrap());
    assert_ne!(graph1.hash(), graph2.hash());
}

// ─── EveryNode 策略 ────────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_every_node() {
    let graph = build_simple_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor =
        GraphExecutor::with_checkpoint(50, mem_store.clone(), CheckpointPolicy::EveryNode, &graph);
    let result = executor.execute(graph.clone(), State::new()).await.unwrap();
    assert!(
        result
            .state
            .get("final")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn test_checkpoint_saved_event() {
    let graph = build_simple_graph();
    let executor = GraphExecutor::with_checkpoint(
        50,
        Arc::new(InMemoryCheckpointStore::new()),
        CheckpointPolicy::EveryNode,
        &graph,
    );
    let mut execution = executor.execute_stream(graph.clone(), State::new());
    let mut checkpoint_saved = false;
    while let Some(ev) = execution.stream.recv().await {
        if matches!(ev, lellm_graph::GraphEvent::CheckpointSaved { .. }) {
            checkpoint_saved = true;
        }
        if matches!(ev, lellm_graph::GraphEvent::GraphComplete { .. }) {
            break;
        }
        if let lellm_graph::GraphEvent::GraphError { ref error, .. } = ev {
            panic!("failed: {}", error);
        }
    }
    assert!(
        checkpoint_saved,
        "expected at least one checkpoint saved event"
    );
}

// ─── BarrierResolved 策略 ──────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_barrier_only() {
    let mut g = GraphBuilder::new("barrier_test");
    g.start("a").end("c");
    g.node(
        "a",
        NodeKind::Task(TaskNode::new("a", |ctx: &mut NodeContext<'_>| {
            ctx.emit_effect(StateEffect::Put("step".into(), serde_json::json!(1u32)));
            Ok(())
        })),
    );
    g.node("b", NodeKind::Barrier(lellm_graph::BarrierNode::new("b")));
    g.node(
        "c",
        NodeKind::Task(TaskNode::new("c", |ctx: &mut NodeContext<'_>| {
            ctx.emit_effect(StateEffect::Put("final".into(), serde_json::json!(true)));
            Ok(())
        })),
    );
    g.edge("a", "b");
    g.edge("b", "c");
    let graph = Arc::new(g.build().unwrap());
    let executor = GraphExecutor::with_checkpoint(
        50,
        Arc::new(InMemoryCheckpointStore::new()),
        CheckpointPolicy::BarrierOnly,
        &graph,
    );
    let mut execution = executor.execute_stream(graph.clone(), State::new());
    let mut count = 0;
    while let Some(ev) = execution.stream.recv().await {
        if let lellm_graph::GraphEvent::CheckpointSaved { .. } = &ev {
            count += 1;
        }
        if let lellm_graph::GraphEvent::BarrierWaiting { ref barrier_id, .. } = ev {
            execution
                .handle
                .decide(barrier_id.clone(), BarrierDecision::Approve)
                .await
                .unwrap();
        }
        if matches!(ev, lellm_graph::GraphEvent::GraphComplete { .. }) {
            break;
        }
        if let lellm_graph::GraphEvent::GraphError { ref error, .. } = ev {
            panic!("failed: {}", error);
        }
    }
    // BarrierOnly 模式：Barrier 解决后 + 图完成时保存
    assert!(
        count >= 1,
        "expected >=1 checkpoint (barrier resolved), got {}",
        count
    );
}

// ─── Manual 策略 ───────────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_manual_no_auto_save() {
    let graph = build_simple_graph();
    let executor = GraphExecutor::with_checkpoint(
        50,
        Arc::new(InMemoryCheckpointStore::new()),
        CheckpointPolicy::Manual,
        &graph,
    );
    let mut execution = executor.execute_stream(graph.clone(), State::new());
    let mut count = 0;
    while let Some(ev) = execution.stream.recv().await {
        if matches!(ev, lellm_graph::GraphEvent::CheckpointSaved { .. }) {
            count += 1;
        }
        if matches!(ev, lellm_graph::GraphEvent::GraphComplete { .. }) {
            break;
        }
        if let lellm_graph::GraphEvent::GraphError { ref error, .. } = ev {
            panic!("failed: {}", error);
        }
    }
    // Manual 模式下不自动保存
    assert_eq!(
        count, 0,
        "expected 0 checkpoints in manual mode, got {}",
        count
    );
}

// ─── 无 Store 时不保存 ──────────────────────────────────────

#[tokio::test]
async fn test_no_store_skips_checkpoint() {
    let graph = build_simple_graph();
    let mut execution = GraphExecutor::new(50).execute_stream(graph.clone(), State::new());
    let mut count = 0;
    while let Some(ev) = execution.stream.recv().await {
        if matches!(ev, lellm_graph::GraphEvent::CheckpointSaved { .. }) {
            count += 1;
        }
        if matches!(ev, lellm_graph::GraphEvent::GraphComplete { .. }) {
            break;
        }
        if let lellm_graph::GraphEvent::GraphError { ref error, .. } = ev {
            panic!("failed: {}", error);
        }
    }
    assert_eq!(
        count, 0,
        "expected 0 checkpoints without store, got {}",
        count
    );
}

// ─── Checkpoint 状态验证 ────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_state_values() {
    let graph = build_simple_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor =
        GraphExecutor::with_checkpoint(50, mem_store.clone(), CheckpointPolicy::EveryNode, &graph);
    let mut execution = executor.execute_stream(graph.clone(), State::new());
    let mut trace_id = None;
    let mut ck_ids = Vec::new();
    while let Some(ev) = execution.stream.recv().await {
        if let lellm_graph::GraphEvent::GraphStart { trace_id: tid } = &ev {
            trace_id = Some(*tid);
        }
        if let lellm_graph::GraphEvent::CheckpointSaved { checkpoint_id, .. } = &ev {
            ck_ids.push(checkpoint_id.clone());
        }
        if matches!(ev, lellm_graph::GraphEvent::GraphComplete { .. }) {
            break;
        }
        if let lellm_graph::GraphEvent::GraphError { ref error, .. } = ev {
            panic!("failed: {}", error);
        }
    }
    let _trace_id = trace_id.expect("should have trace_id");
    assert!(!ck_ids.is_empty());
    let ck = (&*mem_store)
        .load(ck_ids.last().unwrap())
        .await
        .unwrap()
        .expect("exists");
    // 验证 Checkpoint 包含状态
    assert!(ck.state.get("step").is_some() || ck.state.get("done").is_some());
}

// ─── 并发访问 ──────────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_checkpoint_access() {
    let graph = build_simple_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let mut handles = vec![];
    for i in 0..5 {
        let g = graph.clone();
        let s = mem_store.clone();
        let executor = GraphExecutor::with_checkpoint(50, s, CheckpointPolicy::EveryNode, &g);
        handles.push(tokio::spawn(async move {
            let mut state = State::new();
            state.insert(format!("run_id"), serde_json::json!(i));
            executor.execute(g.clone(), state).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert!(
        mem_store.len() >= 5,
        "expected >=5 checkpoints (one per execution), got {}",
        mem_store.len()
    );
}
