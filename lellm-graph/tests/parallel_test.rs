//! ParallelNode 测试。
//!
//! 测试并行执行、Delta 合并、冲突检测、错误策略。

use lellm_graph::State;
use lellm_graph::{
    FlowEvent, GraphBuilder, GraphError, GraphEvent, GraphExecution, NodeContext, NodeKind,
    ParallelErrorStrategy, ParallelNode, SimpleExecutor, StateMutation, StateExt, TaskNode,
};
use std::sync::Arc;

// ─── 基础并行执行 ───────────────────────────────────────────

#[tokio::test]
async fn test_parallel_basic_two_branches() {
    let parallel = ParallelNode::builder()
        .branch(
            "branch_a",
            Arc::new(TaskNode::new("branch_a", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put(
                    "a_result".into(),
                    serde_json::json!("from_a"),
                ));
                Ok(())
            })),
        )
        .branch(
            "branch_b",
            Arc::new(TaskNode::new("branch_b", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put(
                    "b_result".into(),
                    serde_json::json!("from_b"),
                ));
                Ok(())
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_basic");
    let _ = g.start("parallel");
    let _ = g.node("parallel", NodeKind::Parallel(parallel));
    let _ = g.end("parallel");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
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
            Arc::new(TaskNode::new("only", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("single".into(), serde_json::json!(42)));
                Ok(())
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_single");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_u64("single"), Some(42));
}

#[tokio::test]
async fn test_parallel_reads_input_state() {
    let parallel = ParallelNode::builder()
        .branch(
            "reader",
            Arc::new(TaskNode::new("reader", |ctx: &mut NodeContext<'_>| {
                let base: u64 = ctx
                    .state()
                    .get("base")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                ctx.record(StateMutation::Put(
                    "computed".into(),
                    serde_json::json!(base * 2),
                ));
                Ok(())
            })),
        )
        .build();

    let mut initial_state = State::new();
    initial_state.set("base", 21u64);

    let mut g = GraphBuilder::new("parallel_read");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
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
            Arc::new(TaskNode::new("writer_x", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("x".into(), serde_json::json!(1)));
                Ok(())
            })),
        )
        .branch(
            "writer_y",
            Arc::new(TaskNode::new("writer_y", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("y".into(), serde_json::json!(2)));
                Ok(())
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_no_conflict");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_u64("x"), Some(1));
    assert_eq!(result.state.get_u64("y"), Some(2));
}

#[tokio::test]
async fn test_parallel_same_key_conflict() {
    // 两个分支写入同一 key，无 Reducer → 最后写入者胜
    let parallel = ParallelNode::builder()
        .branch(
            "writer_a",
            Arc::new(TaskNode::new("writer_a", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("count".into(), serde_json::json!(1)));
                Ok(())
            })),
        )
        .branch(
            "writer_b",
            Arc::new(TaskNode::new("writer_b", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("count".into(), serde_json::json!(2)));
                Ok(())
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_conflict");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
        .await
        .expect("execution should succeed");

    // 最后写入者胜（writer_b 后执行，其值覆盖 writer_a）
    assert_eq!(result.state.get_u64("count"), Some(2));
}

#[tokio::test]
async fn test_parallel_append_delta_merge() {
    // 两个分支使用 ctx.record() 写入同一 key — 最后写入者胜
    let parallel = ParallelNode::builder()
        .branch(
            "appender_a",
            Arc::new(TaskNode::new("appender_a", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("items".into(), serde_json::json!([1, 2])));
                Ok(())
            })),
        )
        .branch(
            "appender_b",
            Arc::new(TaskNode::new("appender_b", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("items".into(), serde_json::json!([3, 4])));
                Ok(())
            })),
        )
        .build();

    let mut initial_state = State::new();
    initial_state.set("items", serde_json::json!([0]));

    let mut g = GraphBuilder::new("parallel_append");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), initial_state)
        .await
        .expect("execution should succeed");

    let items = result.state.get("items").expect("items should exist");
    let arr = items.as_array().expect("items should be array");
    // 最后写入者胜 — appender_b 的 [3,4] 覆盖 appender_a 的 [1,2]
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0], serde_json::json!(3));
    assert_eq!(arr[1], serde_json::json!(4));
}

// ─── 错误策略 ────────────────────────────────────────────────

#[tokio::test]
async fn test_parallel_fail_fast() {
    let parallel = ParallelNode::builder()
        .branch(
            "ok",
            Arc::new(TaskNode::new("ok", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put(
                    "ok_result".into(),
                    serde_json::json!(true),
                ));
                Ok(())
            })),
        )
        .branch(
            "fail",
            Arc::new(TaskNode::new("fail", |_ctx: &mut NodeContext<'_>| {
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

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
        .await;

    assert!(result.is_err(), "should fail due to failing branch");
}

#[tokio::test]
async fn test_parallel_collect_all() {
    let parallel = ParallelNode::builder()
        .branch(
            "ok",
            Arc::new(TaskNode::new("ok", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put(
                    "ok_result".into(),
                    serde_json::json!(true),
                ));
                Ok(())
            })),
        )
        .branch(
            "fail",
            Arc::new(TaskNode::new("fail", |_ctx: &mut NodeContext<'_>| {
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

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
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
        .branch(
            "fast",
            Arc::new(TaskNode::new("fast", |_ctx: &mut NodeContext<'_>| Ok(()))),
        )
        .branch(
            "also_fast",
            Arc::new(TaskNode::new("also_fast", |_ctx: &mut NodeContext<'_>| {
                Ok(())
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_events");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        SimpleExecutor::default().execute_stream(Arc::new(graph), State::new());

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
    // init -> parallel -> summary
    let parallel = ParallelNode::builder()
        .branch(
            "compute_a",
            Arc::new(TaskNode::new("compute_a", |ctx: &mut NodeContext<'_>| {
                let base: u64 = ctx
                    .state()
                    .get("base")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                ctx.record(StateMutation::Put(
                    "result_a".into(),
                    serde_json::json!(base + 1),
                ));
                Ok(())
            })),
        )
        .branch(
            "compute_b",
            Arc::new(TaskNode::new("compute_b", |ctx: &mut NodeContext<'_>| {
                let base: u64 = ctx
                    .state()
                    .get("base")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                ctx.record(StateMutation::Put(
                    "result_b".into(),
                    serde_json::json!(base * 2),
                ));
                Ok(())
            })),
        )
        .build();

    let mut g = GraphBuilder::new("parallel_pipeline");
    let _ = g.start("init");
    let _ = g.node(
        "init",
        NodeKind::Task(TaskNode::new("init", |ctx: &mut NodeContext<'_>| {
            ctx.record(StateMutation::Put("base".into(), serde_json::json!(10)));
            Ok(())
        })),
    );
    let _ = g.node("parallel", NodeKind::Parallel(parallel));
    let _ = g.node(
        "summary",
        NodeKind::Task(TaskNode::new("summary", |ctx: &mut NodeContext<'_>| {
            let a: u64 = ctx
                .state()
                .get("result_a")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let b: u64 = ctx
                .state()
                .get("result_b")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            ctx.record(StateMutation::Put("total".into(), serde_json::json!(a + b)));
            Ok(())
        })),
    );
    let _ = g.edge("init", "parallel");
    let _ = g.edge("parallel", "summary");
    let _ = g.end("summary");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
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
    let _: ParallelNode = ParallelNode::builder().build();
}

#[tokio::test]
async fn test_parallel_with_label() {
    let parallel = ParallelNode::builder()
        .label("data_processing")
        .branch(
            "step1",
            Arc::new(TaskNode::new("step1", |_ctx: &mut NodeContext<'_>| Ok(()))),
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
        .branch(
            "a",
            Arc::new(TaskNode::new("a", |_ctx: &mut NodeContext<'_>| Ok(()))),
        )
        .build();

    assert_eq!(parallel.error_strategy(), ParallelErrorStrategy::FailFast);
}

#[tokio::test]
async fn test_parallel_three_branches() {
    let parallel = ParallelNode::builder()
        .branch(
            "a",
            Arc::new(TaskNode::new("a", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("v".into(), serde_json::json!("a")));
                Ok(())
            })),
        )
        .branch(
            "b",
            Arc::new(TaskNode::new("b", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("w".into(), serde_json::json!("b")));
                Ok(())
            })),
        )
        .branch(
            "c",
            Arc::new(TaskNode::new("c", |ctx: &mut NodeContext<'_>| {
                ctx.record(StateMutation::Put("x".into(), serde_json::json!("c")));
                Ok(())
            })),
        )
        .build();

    assert_eq!(parallel.branch_count(), 3);

    let mut g = GraphBuilder::new("parallel_three");
    let _ = g.start("p");
    let _ = g.node("p", NodeKind::Parallel(parallel));
    let _ = g.end("p");
    let graph = g.build().expect("build should succeed");

    let result = SimpleExecutor::default()
        .execute(Arc::new(graph), State::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_str("v"), Some("a"));
    assert_eq!(result.state.get_str("w"), Some("b"));
    assert_eq!(result.state.get_str("x"), Some("c"));
}
