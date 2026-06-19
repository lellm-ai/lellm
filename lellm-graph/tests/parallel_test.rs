//! ParallelNode 测试。
//!
//! 测试并行执行、Delta 合并、冲突检测、错误策略。

use lellm_graph::{
    FlowEvent, GraphBuilder, GraphError, GraphEvent, GraphExecution, GraphExecutor, NodeKind,
    ParallelErrorStrategy, ParallelNode, StateDelta, StateExt, TaskNode,
};
use std::collections::HashMap;
use std::sync::Arc;

// ─── 基础并行执行 ───────────────────────────────────────────

#[tokio::test]
async fn test_parallel_basic_two_branches() {
    let parallel = ParallelNode::builder()
        .branch(
            "branch_a",
            Arc::new(TaskNode::new("branch_a", |_state| {
                Ok(vec![StateDelta::put(
                    "a_result",
                    serde_json::json!("from_a"),
                )])
            })),
        )
        .branch(
            "branch_b",
            Arc::new(TaskNode::new("branch_b", |_state| {
                Ok(vec![StateDelta::put(
                    "b_result",
                    serde_json::json!("from_b"),
                )])
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_basic");
    let _ = g.start("parallel");
    let _ = g.node("parallel", NodeKind::Parallel(parallel));
    let _ = g.end("parallel");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(
        result.state.get_str("a_result"),
        Some("from_a"),
        "branch_a delta should be applied"
    );
    assert_eq!(
        result.state.get_str("b_result"),
        Some("from_b"),
        "branch_b delta should be applied"
    );
}

#[tokio::test]
async fn test_parallel_single_branch() {
    let parallel = ParallelNode::builder()
        .branch(
            "only",
            Arc::new(TaskNode::new("only", |_state| {
                Ok(vec![StateDelta::put("single", serde_json::json!(42))])
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_single");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_u64("single"), Some(42));
}

#[tokio::test]
async fn test_parallel_reads_input_state() {
    let parallel = ParallelNode::builder()
        .branch(
            "reader",
            Arc::new(TaskNode::new("reader", |state| {
                let base = state.get_u64("base").unwrap_or(0);
                Ok(vec![StateDelta::put(
                    "computed",
                    serde_json::json!(base * 2),
                )])
            })),
        )
        .build();

    let mut initial_state = HashMap::new();
    initial_state.set("base", 21u64);

    let mut g = GraphBuilder::new("parallel_read");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), initial_state)
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_u64("computed"), Some(42));
    // 原始 base 不变（分支只读）
    assert_eq!(result.state.get_u64("base"), Some(21));
}

// ─── Delta 合并 ──────────────────────────────────────────────

#[tokio::test]
async fn test_parallel_different_keys_no_conflict() {
    let parallel = ParallelNode::builder()
        .branch(
            "writer_x",
            Arc::new(TaskNode::new("writer_x", |_state| {
                Ok(vec![StateDelta::put("x", serde_json::json!(1))])
            })),
        )
        .branch(
            "writer_y",
            Arc::new(TaskNode::new("writer_y", |_state| {
                Ok(vec![StateDelta::put("y", serde_json::json!(2))])
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_no_conflict");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_u64("x"), Some(1));
    assert_eq!(result.state.get_u64("y"), Some(2));
}

#[tokio::test]
async fn test_parallel_same_key_conflict() {
    // 两个分支写入同一 key，无 Reducer → StateConflict
    let parallel = ParallelNode::builder()
        .branch(
            "writer_a",
            Arc::new(TaskNode::new("writer_a", |_state| {
                Ok(vec![
                    StateDelta::put("count", serde_json::json!(1)).with_writer("writer_a"),
                ])
            })),
        )
        .branch(
            "writer_b",
            Arc::new(TaskNode::new("writer_b", |_state| {
                Ok(vec![
                    StateDelta::put("count", serde_json::json!(2)).with_writer("writer_b"),
                ])
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_conflict");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    // 因为 merge_deltas 检测到冲突，返回错误
    assert!(
        result.is_err(),
        "should fail due to state conflict on 'count' key, got {:?}",
        result
    );
}

#[tokio::test]
async fn test_parallel_append_delta_merge() {
    // 两个分支使用 Put 操作写入同一 key — 需要 Reducer::Append 合并
    let parallel = ParallelNode::builder()
        .branch(
            "appender_a",
            Arc::new(TaskNode::new("appender_a", |_state| {
                Ok(vec![StateDelta::put("items", serde_json::json!([1, 2]))])
            })),
        )
        .branch(
            "appender_b",
            Arc::new(TaskNode::new("appender_b", |_state| {
                Ok(vec![StateDelta::put("items", serde_json::json!([3, 4]))])
            })),
        )
        .build();

    let mut initial_state = HashMap::new();
    initial_state.set("items", serde_json::json!([0]));

    let mut g = GraphBuilder::new("parallel_append");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let mut executor = GraphExecutor::default();
    // 注册 Append reducer，允许并行分支追加到 "items"
    executor.register_reducer("items", lellm_graph::Reducer::Append);
    let result = executor
        .execute(Arc::new(graph), initial_state)
        .await
        .expect("execution should succeed");

    let items = result.state.get("items").expect("items should exist");
    let arr = items.as_array().expect("items should be array");
    // 初始 [0] + append [1,2] + append [3,4] = [0,1,2,3,4]
    assert_eq!(arr.len(), 5);
    assert_eq!(arr[0], serde_json::json!(0));
    assert_eq!(arr[1], serde_json::json!(1));
    assert_eq!(arr[2], serde_json::json!(2));
    assert_eq!(arr[3], serde_json::json!(3));
    assert_eq!(arr[4], serde_json::json!(4));
}

// ─── 错误策略 ────────────────────────────────────────────────

#[tokio::test]
async fn test_parallel_fail_fast() {
    let parallel = ParallelNode::builder()
        .branch(
            "ok",
            Arc::new(TaskNode::new("ok", |_state| {
                Ok(vec![StateDelta::put("ok_result", serde_json::json!(true))])
            })),
        )
        .branch(
            "fail",
            Arc::new(TaskNode::new("fail", |_state| {
                Err(GraphError::Terminal(
                    lellm_graph::TerminalError::NodeExecutionFailed {
                        node: "fail".into(),
                        source: "intentional failure".into(),
                    },
                ))
            })),
        )
        .error_strategy(ParallelErrorStrategy::FailFast)
        .build();

    let mut g = GraphBuilder::new("parallel_fail_fast");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    assert!(result.is_err(), "should fail due to failing branch");
}

#[tokio::test]
async fn test_parallel_collect_all() {
    let parallel = ParallelNode::builder()
        .branch(
            "ok",
            Arc::new(TaskNode::new("ok", |_state| {
                Ok(vec![StateDelta::put("ok_result", serde_json::json!(true))])
            })),
        )
        .branch(
            "fail",
            Arc::new(TaskNode::new("fail", |_state| {
                Err(GraphError::Terminal(
                    lellm_graph::TerminalError::NodeExecutionFailed {
                        node: "fail".into(),
                        source: "intentional failure".into(),
                    },
                ))
            })),
        )
        .error_strategy(ParallelErrorStrategy::CollectAll)
        .build();

    let mut g = GraphBuilder::new("parallel_collect_all");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    assert!(
        result.is_err(),
        "should fail even with CollectAll when a branch fails"
    );
}

// ─── 流式事件 ────────────────────────────────────────────────

#[tokio::test]
async fn test_parallel_emits_events() {
    let parallel = ParallelNode::builder()
        .label("my_parallel")
        .branch("fast", Arc::new(TaskNode::new("fast", |_state| Ok(vec![]))))
        .branch(
            "also_fast",
            Arc::new(TaskNode::new("also_fast", |_state| Ok(vec![]))),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_events");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

    drop(handle);

    let mut has_parallel_started = false;
    let mut has_parallel_completed = false;
    let mut branch_completed_count = 0;

    while let Some(event) = stream.recv().await {
        match &event {
            GraphEvent::Node {
                event: FlowEvent::ParallelStarted { branch_count, .. },
                ..
            } => {
                has_parallel_started = true;
                assert_eq!(*branch_count, 2);
            }
            GraphEvent::Node {
                event: FlowEvent::BranchCompleted { .. },
                ..
            } => {
                branch_completed_count += 1;
            }
            GraphEvent::Node {
                event: FlowEvent::ParallelCompleted { .. },
                ..
            } => {
                has_parallel_completed = true;
            }
            _ => {}
        }
    }

    assert!(has_parallel_started, "should emit ParallelStarted");
    assert!(has_parallel_completed, "should emit ParallelCompleted");
    assert_eq!(branch_completed_count, 2, "should emit 2 BranchCompleted");
}

// ─── Pipeline 集成 ───────────────────────────────────────────

#[tokio::test]
async fn test_parallel_in_pipeline() {
    // init → parallel → summary
    let parallel = ParallelNode::builder()
        .branch(
            "compute_a",
            Arc::new(TaskNode::new("compute_a", |state| {
                let base = state.get_u64("base").unwrap_or(0);
                Ok(vec![StateDelta::put(
                    "result_a",
                    serde_json::json!(base + 1),
                )])
            })),
        )
        .branch(
            "compute_b",
            Arc::new(TaskNode::new("compute_b", |state| {
                let base = state.get_u64("base").unwrap_or(0);
                Ok(vec![StateDelta::put(
                    "result_b",
                    serde_json::json!(base * 2),
                )])
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_pipeline");
    let _ = g.start("init");
    let _ = g.node(
        "init",
        NodeKind::Task(TaskNode::new("init", |_state| {
            Ok(vec![StateDelta::put("base", serde_json::json!(10))])
        })),
    );
    let _ = g.node("parallel", NodeKind::Parallel(parallel));
    let _ = g.node(
        "summary",
        NodeKind::Task(TaskNode::new("summary", |state| {
            let a = state.get_u64("result_a").unwrap_or(0);
            let b = state.get_u64("result_b").unwrap_or(0);
            Ok(vec![StateDelta::put("total", serde_json::json!(a + b))])
        })),
    );
    let _ = g.edge("init", "parallel");
    let _ = g.edge("parallel", "summary");
    let _ = g.end("summary");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_u64("base"), Some(10));
    assert_eq!(result.state.get_u64("result_a"), Some(11)); // 10 + 1
    assert_eq!(result.state.get_u64("result_b"), Some(20)); // 10 * 2
    assert_eq!(result.state.get_u64("total"), Some(31)); // 11 + 20
}

// ─── 边界情况 ────────────────────────────────────────────────

#[test]
#[should_panic(expected = "at least one branch")]
fn test_parallel_no_branches_panics() {
    let _ = ParallelNode::builder().build();
}

#[tokio::test]
async fn test_parallel_with_label() {
    let parallel = ParallelNode::builder()
        .label("data_processing")
        .branch(
            "step1",
            Arc::new(TaskNode::new("step1", |_state| Ok(vec![]))),
        )
        .build();

    assert_eq!(parallel.label(), Some("data_processing"));
    assert_eq!(parallel.branch_count(), 1);
    assert_eq!(parallel.branch_names(), vec!["step1"]);
    assert_eq!(parallel.error_strategy(), ParallelErrorStrategy::FailFast);
}

#[tokio::test]
async fn test_parallel_default_error_strategy() {
    let parallel = ParallelNode::builder()
        .branch("a", Arc::new(TaskNode::new("a", |_state| Ok(vec![]))))
        .build();

    assert_eq!(parallel.error_strategy(), ParallelErrorStrategy::FailFast);
}

#[tokio::test]
async fn test_parallel_three_branches() {
    let parallel = ParallelNode::builder()
        .branch(
            "a",
            Arc::new(TaskNode::new("a", |_state| {
                Ok(vec![StateDelta::put("v", serde_json::json!("a"))])
            })),
        )
        .branch(
            "b",
            Arc::new(TaskNode::new("b", |_state| {
                Ok(vec![StateDelta::put("w", serde_json::json!("b"))])
            })),
        )
        .branch(
            "c",
            Arc::new(TaskNode::new("c", |_state| {
                Ok(vec![StateDelta::put("x", serde_json::json!("c"))])
            })),
        )
        .build();

    assert_eq!(parallel.branch_count(), 3);

    let mut g = GraphBuilder::new("parallel_three");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_str("v"), Some("a"));
    assert_eq!(result.state.get_str("w"), Some("b"));
    assert_eq!(result.state.get_str("x"), Some("c"));
}
