//! Checkpoint 恢复测试 — v04 #2
//!
//! 验证 Checkpoint 能正确保存和恢复 Agent 中间状态。

use async_trait::async_trait;
use lellm_graph::{
    CheckpointPolicy, CheckpointStore, GraphBuilder, GraphExecutor, InMemoryCheckpointStore,
    NodeContext, NodeKind, State, TaskNode,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn to_store(s: Arc<InMemoryCheckpointStore>) -> Arc<dyn lellm_graph::CheckpointStore> {
    s
}

/// 自定义节点 — 使用 NodeContext API 写入状态
fn make_set_node(name: &str, key: &str, value: &str) -> TaskNode {
    let k = key.to_string();
    let v = value.to_string();
    TaskNode::new(name, move |ctx: &mut NodeContext<'_>| {
        ctx.set(&k, v.clone());
        Ok(())
    })
}

/// 构建一个简单的 3 节点线性图：set_a → set_b → set_c
fn build_linear_graph() -> Arc<lellm_graph::Graph> {
    let mut g = GraphBuilder::new("linear");
    g.start("set_a").end("set_c");
    g.node("set_a", NodeKind::Task(make_set_node("set_a", "step", "a")));
    g.node("set_b", NodeKind::Task(make_set_node("set_b", "step", "b")));
    g.node("set_c", NodeKind::Task(make_set_node("set_c", "step", "c")));
    g.edge("set_a", "set_b");
    g.edge("set_b", "set_c");
    Arc::new(g.build().expect("build"))
}

// ─── 基本保存/恢复 ────────────────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_save_and_load() {
    let graph = build_linear_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );

    let result = executor.execute(graph.clone(), State::new()).await.unwrap();

    // 图执行完成，step 应为 c
    assert_eq!(result.state.get("step").and_then(|v| v.as_str()), Some("c"));

    // 至少有一个 Checkpoint 被保存（ExecutionCompleted 触发）
    assert!(
        !mem_store.is_empty(),
        "should have saved at least one checkpoint"
    );
}

#[tokio::test]
async fn test_checkpoint_state_preserved() {
    let graph = build_linear_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );

    let mut state = State::new();
    state.insert("input".into(), serde_json::json!("test_value"));
    let result = executor.execute(graph.clone(), state).await.unwrap();

    // 从 store 加载最新 Checkpoint
    let ck = mem_store
        .load_latest(&result.trace_id)
        .await
        .unwrap()
        .expect("should have checkpoint");

    // 验证 Checkpoint 中保留了最终状态
    assert_eq!(ck.state.get("step").and_then(|v| v.as_str()), Some("c"));
    assert_eq!(
        ck.state.get("input").and_then(|v| v.as_str()),
        Some("test_value")
    );
}

// ─── 从 Checkpoint 恢复执行 ───────────────────────────────────────

#[tokio::test]
async fn test_resume_from_checkpoint() {
    let graph = build_linear_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );

    // 第一次执行
    let result1 = executor.execute(graph.clone(), State::new()).await.unwrap();

    // 从 Checkpoint 恢复
    let executor2 = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );

    let mut result2 = executor2
        .resume_from(mem_store.as_ref(), &result1.trace_id, &graph)
        .await
        .unwrap();

    // 消费恢复后的执行流
    let mut completed = false;
    while let Some(event) = result2.stream.recv().await {
        match event {
            lellm_graph::GraphEvent::GraphComplete { .. } => {
                completed = true;
                break;
            }
            lellm_graph::GraphEvent::GraphError { ref error, .. } => {
                panic!("resume failed: {}", error);
            }
            _ => {}
        }
    }

    assert!(completed, "should complete after resume");
}

// ─── 状态一致性 ──────────────────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_state_consistency() {
    let graph = build_linear_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());

    // 有 Checkpoint 执行
    let executor1 = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );
    let result1 = executor1.execute(graph.clone(), State::new()).await.unwrap();

    // 无 Checkpoint 执行
    let executor2 = GraphExecutor::new(50);
    let result2 = executor2.execute(graph.clone(), State::new()).await.unwrap();

    // 两种执行路径的最终状态应一致
    assert_eq!(
        result1.state.get("step").and_then(|v| v.as_str()),
        result2.state.get("step").and_then(|v| v.as_str()),
        "checkpoint and non-checkpoint execution should produce same state"
    );
}

// ─── Typed State 序列化/恢复 ──────────────────────────────────────

/// 验证 Typed State 能通过 Checkpoint 正确序列化/恢复。
#[tokio::test]
async fn test_checkpoint_typed_state_roundtrip() {
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestTypedState {
        messages: Vec<String>,
        counter: u64,
    }

    let typed = TestTypedState {
        messages: vec!["msg1".into(), "msg2".into()],
        counter: 42,
    };

    // 构建写入 Typed State 的图
    let typed_json = serde_json::to_value(&typed).unwrap();
    let mut g = GraphBuilder::new("typed_state_test");
    g.start("write").end("read");
    g.node(
        "write",
        NodeKind::Task(TaskNode::new("write", move |ctx: &mut NodeContext<'_>| {
            ctx.set("typed_state", typed_json.clone());
            Ok(())
        })),
    );
    g.node(
        "read",
        NodeKind::Task(TaskNode::new("read", |ctx: &mut NodeContext<'_>| {
            let restored: TestTypedState = ctx
                .get("typed_state")
                .expect("typed state should exist");
            assert_eq!(restored.counter, 42);
            assert_eq!(restored.messages.len(), 2);
            ctx.set("verified", true);
            Ok(())
        })),
    );
    g.edge("write", "read");
    let graph = Arc::new(g.build().expect("build"));

    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );

    let result = executor.execute(graph.clone(), State::new()).await.unwrap();

    // 验证图执行通过（read 节点的 assert 没有 panic）
    assert_eq!(result.state.get("verified").and_then(|v| v.as_bool()), Some(true));

    // 从 Checkpoint 加载，验证 Typed State 完整保留
    let ck = mem_store
        .load_latest(&result.trace_id)
        .await
        .unwrap()
        .expect("should have checkpoint");

    let restored: TestTypedState = serde_json::from_value(
        ck.state.get("typed_state").cloned().expect("typed_state in checkpoint"),
    )
    .expect("typed state should survive checkpoint roundtrip");

    assert_eq!(restored.counter, 42);
    assert_eq!(restored.messages, vec!["msg1", "msg2"]);
}

// ─── 多次 Checkpoint 列表 ────────────────────────────────────────

#[tokio::test]
async fn test_checkpoint_list_ordering() {
    let graph = build_linear_graph();
    let mem_store = Arc::new(InMemoryCheckpointStore::new());
    let executor = GraphExecutor::with_checkpoint(
        50,
        to_store(mem_store.clone()),
        CheckpointPolicy::conservative(),
        &graph,
    );

    let result = executor.execute(graph.clone(), State::new()).await.unwrap();

    // 列出该 trace 的所有 Checkpoint
    let ids = mem_store.list(&result.trace_id).await.unwrap();

    // 至少有一个（ExecutionCompleted）
    assert!(!ids.is_empty(), "should have checkpoints for this trace");

    // 能逐个加载
    for id in &ids {
        let ck = mem_store.load(id).await.unwrap().expect("checkpoint exists");
        assert_eq!(ck.parent_trace_id, result.trace_id);
    }
}
