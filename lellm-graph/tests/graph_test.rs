use lellm_graph::{
    array_reducer, BarrierDecision, BarrierDefaultAction, BarrierNode, GraphBuilder, GraphError,
    GraphEvent, GraphExecutor, LoopNode, NodeKind, State, StateExt, SubGraph, TaskNode, TraceId,
};
use std::collections::HashMap;
use std::time::Duration;

/// Helper: 构建 Graph 并返回 Result，配合 `?` 使用链式 Builder API。
fn build_graph<F>(name: &str, f: F) -> Result<lellm_graph::Graph, GraphError>
where
    F: FnOnce(&mut GraphBuilder) -> Result<(), GraphError>,
{
    let mut g = GraphBuilder::new(name);
    f(&mut g)?;
    g.build()
}

#[tokio::test]
async fn test_linear_pipeline() {
    let graph = build_graph("linear", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                state.insert("step".into(), serde_json::json!("a"));
                Ok(())
            })),
        );
        let _ = g.node(
            "b",
            NodeKind::Task(TaskNode::new("b", |state| {
                state.insert("step".into(), serde_json::json!("b"));
                Ok(())
            })),
        );
        let _ = g.node(
            "c",
            NodeKind::Task(TaskNode::new("c", |state| {
                state.insert("step".into(), serde_json::json!("c"));
                Ok(())
            })),
        );
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "c");
        let _ = g.end("c");
        Ok(())
    })
    .expect("build should succeed");

    let initial_state = HashMap::new();
    let result = GraphExecutor::default()
        .execute(&graph, initial_state)
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("step").unwrap(), &serde_json::json!("c"));
    assert_eq!(result.execution_log.len(), 3);
}

#[tokio::test]
async fn test_condition_branching() {
    let graph = GraphBuilder::new("condition")
        .start("check")
        .node(
            "check",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("check")
                    .branch("yes", |s| s.get("flag").and_then(|v| v.as_bool()).unwrap_or(false))
                    .branch("no", |_| true)
                    .build(),
            ),
        )
        .node(
            "yes",
            NodeKind::Task(TaskNode::new("yes", |state| {
                state.insert("result".into(), serde_json::json!("yes"));
                Ok(())
            })),
        )
        .node(
            "no",
            NodeKind::Task(TaskNode::new("no", |state| {
                state.insert("result".into(), serde_json::json!("no"));
                Ok(())
            })),
        )
        .edge("yes", "yes_end")
        .edge("no", "no_end")
        .node(
            "yes_end",
            NodeKind::Task(TaskNode::new("yes_end", |_| Ok(()))),
        )
        .node(
            "no_end",
            NodeKind::Task(TaskNode::new("no_end", |_| Ok(()))),
        )
        .end("yes_end")
        .build()
        .expect("build should succeed");

    let mut initial_state = HashMap::new();
    initial_state.insert("flag".into(), serde_json::json!(true));
    let result = GraphExecutor::default()
        .execute(&graph, initial_state)
        .await
        .expect("execution should succeed");

    assert_eq!(
        result.state.get("result").unwrap(),
        &serde_json::json!("yes")
    );
}

#[tokio::test]
async fn test_task_node_error() {
    let graph = GraphBuilder::new("error")
        .start("fail")
        .node(
            "fail",
            NodeKind::Task(TaskNode::new("fail", |_| {
                Err(lellm_graph::GraphError::StateError("boom".into()))
            })),
        )
        .end("fail")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await;
    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::StateError(msg) => assert_eq!(msg, "boom"),
        other => panic!("expected StateError, got: {other}"),
    }
}

// ─── 有环图测试（路线 B）─────────────────────────────────────────

/// 有环图现在可以正常构建 — 不再被 detect_cycle 拦截。
#[test]
fn test_cyclic_graph_allowed() {
    let result = GraphBuilder::new("cycle")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .edge("b", "a")
        .end("b")
        .build();

    assert!(result.is_ok(), "cyclic graph should be allowed to build");
}

/// 有环图执行时，max_steps 熔断器防止无限循环。
#[tokio::test]
async fn test_cyclic_graph_steps_exceeded() {
    let graph = GraphBuilder::new("infinite_cycle")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .node("done", NodeKind::Task(TaskNode::new("done", |_| Ok(()))))
        .edge("a", "b")
        .edge("b", "a")
        .end("done")
        .build()
        .expect("cyclic graph should build");

    let executor = GraphExecutor::new(5);
    let result = executor.execute(&graph, HashMap::new()).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::StepsExceeded { limit } => assert_eq!(limit, 5),
        other => panic!("expected StepsExceeded, got: {other}"),
    }
}

/// 有环图 + edge_if 条件回跳 — 最核心的 Agent 编排模式。
#[tokio::test]
async fn test_cyclic_graph_with_edge_if_exit() {
    let graph = GraphBuilder::new("cyclic_with_exit")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .edge("a", "b")
        .edge_if("b", "a", |s| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 3)
                .unwrap_or(true)
        })
        .edge("b", "end")
        .end("end")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(3));
    assert_eq!(result.execution_log.len(), 7);
}

/// ConditionNode 回跳 — 复杂多路分支场景的语法糖。
#[tokio::test]
async fn test_condition_node_back_jump() {
    let graph = GraphBuilder::new("cond_back_jump")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .node(
            "route",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("route")
                    .branch("a", |s| {
                        s.get("count")
                            .and_then(|v| v.as_u64())
                            .map(|c| c < 2)
                            .unwrap_or(true)
                    })
                    .branch("end", |_| true)
                    .build(),
            ),
        )
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .edge("a", "route")
        .end("end")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(2));
}

#[test]
fn test_missing_node() {
    let result = GraphBuilder::new("missing")
        .start("a")
        .edge("a", "nonexistent")
        .end("nonexistent")
        .build();

    assert!(result.is_err());
}

#[test]
fn test_missing_start() {
    let result = GraphBuilder::new("no_start")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .end("a")
        .build();

    assert!(result.is_err());
}

#[test]
fn test_missing_end() {
    let result = GraphBuilder::new("no_end")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .build();

    assert!(result.is_err());
}

#[tokio::test]
async fn test_execution_log() {
    let graph = GraphBuilder::new("log")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .end("b")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.execution_log.len(), 2);
    assert!(result.execution_log.iter().all(|e| e.success));
    assert!(result.duration.as_nanos() > 0);
}

/// LoopNode — 独立迭代保护的封装场景。
#[tokio::test]
async fn test_loop_node_basic() {
    let body = SubGraph {
        nodes: vec![Box::new(TaskNode::new("increment", |state| {
            let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            state.insert("count".into(), serde_json::json!(count + 1));
            Ok(())
        }))],
        edges: vec![],
    };

    let loop_node = LoopNode::new(
        "counter",
        body,
        |s| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 3)
                .unwrap_or(true)
        },
        10,
    );

    let graph = GraphBuilder::new("loop_test")
        .start("loop")
        .node("loop", NodeKind::Loop(Box::new(loop_node)))
        .end("loop")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(3));
}

/// LoopNode 超限 — 独立于全局 max_steps。
#[tokio::test]
async fn test_loop_node_limit_exceeded() {
    let body = SubGraph {
        nodes: vec![Box::new(TaskNode::new("no_op", |_| Ok(())))],
        edges: vec![],
    };

    let loop_node = LoopNode::new("infinite", body, |_| true, 2);

    let graph = GraphBuilder::new("loop_limit")
        .start("loop")
        .node("loop", NodeKind::Loop(Box::new(loop_node)))
        .end("loop")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::LoopLimitExceeded { limit } => assert_eq!(limit, 2),
        other => panic!("expected LoopLimitExceeded, got: {other}"),
    }
}

// ─── BarrierNode 测试（Human-in-the-loop）────────────────────────

/// BarrierNode 在阻塞模式下必须报错。
#[tokio::test]
async fn test_barrier_blocked_mode_error() {
    let graph = GraphBuilder::new("barrier_blocked")
        .start("barrier")
        .node("barrier", NodeKind::Barrier(BarrierNode::new("review")))
        .end("barrier")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::InvalidGraph(msg) => {
            assert!(
                msg.contains("execute_stream"),
                "should guide user to stream mode"
            );
        }
        other => panic!("expected InvalidGraph, got: {other}"),
    }
}

/// BarrierNode 流式模式 — Approve 决策（使用 GraphHandle::decide）。
#[tokio::test]
async fn test_barrier_approve() {
    let graph = GraphBuilder::new("approve_flow")
        .start("barrier")
        .node("barrier", NodeKind::Barrier(BarrierNode::new("review")))
        .node("after", NodeKind::Task(TaskNode::new("after", |_| Ok(()))))
        .edge("barrier", "after")
        .end("after")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let (mut stream, handle) = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { node_name, barrier_id } => {
                assert_eq!(node_name, "review");
                let _ = handle.decide(barrier_id, BarrierDecision::Approve).await;
            }
            GraphEvent::GraphComplete { result } => {
                assert_eq!(
                    result.state.get("review.approved").unwrap(),
                    &serde_json::json!(true),
                    "approve marker should be in state"
                );
                break;
            }
            _ => {}
        }
    }
}

/// BarrierNode — Reject 决策 + edge_if 回跳。
#[tokio::test]
async fn test_barrier_reject_with_back_jump() {
    let graph = GraphBuilder::new("reject_flow")
        .start("task")
        .node(
            "task",
            NodeKind::Task(TaskNode::new("task", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .node("review", NodeKind::Barrier(BarrierNode::new("review")))
        .edge("task", "review")
        .edge_if("review", "task", |s| s.get("review.reject_reason").is_some())
        .node("done", NodeKind::Task(TaskNode::new("done", |_| Ok(()))))
        .edge("review", "done")
        .end("done")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let (mut stream, handle) = GraphExecutor::default().execute_stream(graph, HashMap::new());

    let mut reject_count = 0;
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { node_name, barrier_id } => {
                assert_eq!(node_name, "review");
                reject_count += 1;
                if reject_count == 1 {
                    let _ = handle
                        .decide(
                            barrier_id,
                            BarrierDecision::Reject {
                                reason: "需要改进".into(),
                            },
                        )
                        .await;
                } else {
                    let _ = handle.decide(barrier_id, BarrierDecision::Approve).await;
                }
            }
            GraphEvent::GraphComplete { result } => {
                assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(2));
                assert_eq!(
                    result.state.get("review.approved").unwrap(),
                    &serde_json::json!(true)
                );
                break;
            }
            _ => {}
        }
    }
}

/// BarrierNode — Modify 决策，修改 State 后继续。
#[tokio::test]
async fn test_barrier_modify() {
    let graph = GraphBuilder::new("modify_flow")
        .start("barrier")
        .node("barrier", NodeKind::Barrier(BarrierNode::new("input")))
        .node("after", NodeKind::Task(TaskNode::new("after", |_| Ok(()))))
        .edge("barrier", "after")
        .end("after")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let (mut stream, handle) = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { barrier_id, .. } => {
                let _ = handle
                    .decide(
                        barrier_id,
                        BarrierDecision::Modify {
                            key: "user_input".into(),
                            value: serde_json::json!("人工补充的数据"),
                        },
                    )
                    .await;
            }
            GraphEvent::GraphComplete { result } => {
                assert_eq!(
                    result.state.get("user_input").unwrap(),
                    &serde_json::json!("人工补充的数据")
                );
                break;
            }
            _ => {}
        }
    }
}

/// BarrierNode — 超时自动 Reject。
#[tokio::test]
async fn test_barrier_timeout() {
    let graph = GraphBuilder::new("timeout_flow")
        .start("barrier")
        .node(
            "barrier",
            NodeKind::Barrier(
                BarrierNode::new("review")
                    .timeout(Duration::from_millis(100))
                    .default_action(BarrierDefaultAction::Reject),
            ),
        )
        .node("after", NodeKind::Task(TaskNode::new("after", |_| Ok(()))))
        .edge("barrier", "after")
        .end("after")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let (mut stream, _handle) = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { .. } => {
                // 故意不发送决策 — 让 BarrierNode 超时
            }
            GraphEvent::GraphComplete { result } => {
                assert!(
                    result.state.get("review.reject_reason").is_some(),
                    "reject reason should be set on timeout"
                );
                break;
            }
            GraphEvent::GraphError { ref error } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }
}

/// BarrierNode — Reroute 决策，跳转到指定节点。
#[tokio::test]
async fn test_barrier_reroute() {
    let graph = GraphBuilder::new("reroute_flow")
        .start("barrier")
        .node("barrier", NodeKind::Barrier(BarrierNode::new("route")))
        .node(
            "path_a",
            NodeKind::Task(TaskNode::new("path_a", |state| {
                state.insert("path".into(), serde_json::json!("A"));
                Ok(())
            })),
        )
        .node(
            "path_b",
            NodeKind::Task(TaskNode::new("path_b", |state| {
                state.insert("path".into(), serde_json::json!("B"));
                Ok(())
            })),
        )
        .edge("barrier", "path_a")
        .edge("path_a", "end")
        .edge("path_b", "end")
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .end("end")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let (mut stream, handle) = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { barrier_id, .. } => {
                let _ = handle
                    .decide(
                        barrier_id,
                        BarrierDecision::Reroute {
                            target: "path_b".into(),
                        },
                    )
                    .await;
            }
            GraphEvent::GraphComplete { result } => {
                assert_eq!(
                    result.state.get("path").unwrap(),
                    &serde_json::json!("B"),
                    "should have taken path B via reroute"
                );
                break;
            }
            _ => {}
        }
    }
}

// ─── StateExt 测试 ──────────────────────────────────────────────

#[test]
fn test_state_ext_getters() {
    let mut state = State::new();
    state.insert("name".into(), serde_json::json!("hello"));
    state.insert("count".into(), serde_json::json!(42));
    state.insert("enabled".into(), serde_json::json!(true));
    state.insert("score".into(), serde_json::json!(3.14));

    assert_eq!(state.get_str("name"), Some("hello"));
    assert_eq!(state.get_u64("count"), Some(42));
    assert_eq!(state.get_bool("enabled"), Some(true));
    assert_eq!(state.get_f64("score"), Some(3.14));
    assert_eq!(state.get_str("missing"), None);
    assert!(state.contains("name"));
    assert!(!state.contains("missing"));
}

#[test]
fn test_state_ext_set() {
    let mut state = State::new();
    state.set("count", 42u64);
    state.set("name", "hello");
    state.set("enabled", true);

    assert_eq!(state.get_u64("count"), Some(42));
    assert_eq!(state.get_str("name"), Some("hello"));
    assert_eq!(state.get_bool("enabled"), Some(true));
}

#[test]
fn test_state_ext_remove() {
    let mut state = State::new();
    state.insert("key".into(), serde_json::json!("value"));
    let removed = state.remove("key");
    assert!(removed.is_some());
    assert!(!state.contains("key"));
}

#[test]
fn test_state_ext_get_json() {
    let mut state = State::new();
    state.set(
        "config",
        serde_json::json!({"nested": {"key": "value"}}),
    );

    let config: serde_json::Value = state.get_json("config").unwrap();
    assert_eq!(config["nested"]["key"], "value");

    let err = state.get_json::<String>("missing");
    assert!(err.is_err());
}

#[test]
fn test_state_ext_append_array() {
    let mut state = State::new();
    state.append_array("items", serde_json::json!([1, 2])).unwrap();
    state.append_array("items", serde_json::json!([3, 4])).unwrap();

    let items = state.get("items").unwrap();
    assert_eq!(items, &serde_json::json!([1, 2, 3, 4]));
}

#[test]
fn test_state_ext_reduce() {
    let mut state = State::new();
    state.insert("items".into(), serde_json::json!([1, 2]));
    state.reduce("items", serde_json::json!([3, 4]), &array_reducer())
        .unwrap();

    let items = state.get("items").unwrap();
    assert_eq!(items, &serde_json::json!([1, 2, 3, 4]));
}

// ─── Edge max_visits 测试 ───────────────────────────────────────

/// 边级循环预算 — 超过 max_visits 返回 EdgeLimitExceeded。
#[tokio::test]
async fn test_edge_max_visits_exceeded() {
    let graph = GraphBuilder::new("edge_limit")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .edge("a", "b")
        // 条件边 + max_visits=2：最多回跳 2 次
        .edge_if_max_visits("b", "a", |s| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 10)
                .unwrap_or(true)
        }, 2)
        .edge("b", "end")
        .end("end")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await;

    // b→a 边走了 2 次后达到 max_visits，第 3 次应报错
    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::EdgeLimitExceeded { edge, limit } => {
            assert_eq!(edge, "b→a");
            assert_eq!(limit, 2);
        }
        other => panic!("expected EdgeLimitExceeded, got: {other}"),
    }
}

/// 边级预算未超限 — 正常退出。
#[tokio::test]
async fn test_edge_max_visits_ok() {
    let graph = GraphBuilder::new("edge_limit_ok")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        )
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .edge("a", "b")
        .edge_if_max_visits("b", "a", |s| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 2)
                .unwrap_or(true)
        }, 5)
        .edge("b", "end")
        .end("end")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(2));
}

// ─── Cycle Analysis 测试 ────────────────────────────────────────

/// DAG 无环。
#[test]
fn test_analyze_cycles_dag() {
    let graph = GraphBuilder::new("dag")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .end("b")
        .build()
        .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    assert!(!analysis.has_cycles);
    assert!(analysis.cycles.is_empty());
    assert!(analysis.all_protected());
}

/// 有环图检测到环。
#[test]
fn test_analyze_cycles_detected() {
    let graph = GraphBuilder::new("cycle")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .node("c", NodeKind::Task(TaskNode::new("c", |_| Ok(()))))
        .edge("a", "b")
        .edge("b", "c")
        .edge("c", "a")
        .end("a")
        .build()
        .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    assert!(analysis.has_cycles);
    assert!(!analysis.cycles.is_empty());
}

/// 有环图 + max_visits 保护。
#[test]
fn test_analyze_cycles_protected() {
    let graph = GraphBuilder::new("protected_cycle")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .edge_if_max_visits("b", "a", |_| true, 5)
        .edge("b", "end")
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .end("end")
        .build()
        .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    assert!(analysis.has_cycles);
    assert!(analysis.all_protected());
}

/// analyze_cycles 生成诊断报告。
#[test]
fn test_analyze_cycles_report() {
    let graph = GraphBuilder::new("report_test")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .edge("b", "a")
        .end("a")
        .build()
        .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    let report = analysis.report();
    assert!(report.contains("Cycle Analysis"));
    assert!(report.contains("cycle"));
}

// ─── TraceId 测试 ───────────────────────────────────────────────

/// TraceId 生成唯一 ID。
#[test]
fn test_trace_id_uniqueness() {
    let id1 = TraceId::new();
    let id2 = TraceId::new();
    assert_ne!(id1.to_string(), id2.to_string());
}

/// 流式执行事件包含 trace_id。
#[tokio::test]
async fn test_stream_has_trace_id() {
    let graph = GraphBuilder::new("trace_test")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .end("b")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let (mut stream, _handle) = GraphExecutor::default().execute_stream(graph, HashMap::new());

    let mut trace_ids = Vec::new();
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::NodeStart { trace_id, .. } => {
                trace_ids.push(trace_id);
            }
            GraphEvent::NodeEnd { trace_id, .. } => {
                trace_ids.push(trace_id);
            }
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            _ => {}
        }
    }

    // 至少有两个节点，每个有 start + end，共 4 个 trace_id
    assert!(trace_ids.len() >= 4);
}
