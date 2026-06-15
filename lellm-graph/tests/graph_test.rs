use lellm_graph::{
    BarrierDecision, BarrierDefaultAction, BarrierNode, BuildError, GraphBuilder, GraphError,
    GraphEvent, GraphExecutor, LoopNode, NodeKind, State, StateExt, SubGraph, TaskNode, TerminalError, TraceId,
    array_reducer, EdgePolicy, EdgeExceededStrategy,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Helper: 构建 Graph 并返回 Result，配合 `?` 使用链式 Builder API。
fn build_graph<F>(name: &str, f: F) -> Result<lellm_graph::Graph, BuildError>
where
    F: FnOnce(&mut GraphBuilder) -> Result<(), BuildError>,
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
        .execute(Arc::new(graph), initial_state)
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("step").unwrap(), &serde_json::json!("c"));
    assert_eq!(result.execution_log.len(), 3);
}

#[tokio::test]
async fn test_condition_branching() {
    let graph = build_graph("condition", |g| {
        let _ = g.start("check");
        let _ = g.node(
            "check",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("check")
                    .branch("yes", |s| {
                        s.get("flag").and_then(|v| v.as_bool()).unwrap_or(false)
                    })
                    .branch("no", |_| true)
                    .build(),
            ),
        );
        let _ = g.node(
            "yes",
            NodeKind::Task(TaskNode::new("yes", |state| {
                state.insert("result".into(), serde_json::json!("yes"));
                Ok(())
            })),
        );
        let _ = g.node(
            "no",
            NodeKind::Task(TaskNode::new("no", |state| {
                state.insert("result".into(), serde_json::json!("no"));
                Ok(())
            })),
        );
        let _ = g.edge("check", "yes");
        let _ = g.edge("check", "no");
        let _ = g.edge("yes", "yes_end");
        let _ = g.edge("no", "no_end");
        let _ = g.node(
            "yes_end",
            NodeKind::Task(TaskNode::new("yes_end", |_| Ok(()))),
        );
        let _ = g.node(
            "no_end",
            NodeKind::Task(TaskNode::new("no_end", |_| Ok(()))),
        );
        let _ = g.end("yes_end");
        Ok(())
    })
    .expect("build should succeed");

    let mut initial_state = HashMap::new();
    initial_state.insert("flag".into(), serde_json::json!(true));
    let result = GraphExecutor::default()
        .execute(Arc::new(graph), initial_state)
        .await
        .expect("execution should succeed");

    assert_eq!(
        result.state.get("result").unwrap(),
        &serde_json::json!("yes")
    );
}

#[tokio::test]
async fn test_task_node_error() {
    let graph = build_graph("error", |g| {
        let _ = g.start("fail");
        let _ = g.node(
            "fail",
            NodeKind::Task(TaskNode::new("fail", |_| {
                Err(GraphError::Terminal(TerminalError::StateError("boom".into())))
            })),
        );
        let _ = g.end("fail");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;
    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::Terminal(TerminalError::StateError(msg)) => assert_eq!(msg, "boom"),
        other => panic!("expected StateError, got: {other}"),
    }
}

// ─── 有环图测试（路线 B）─────────────────────────────────────────

/// 有环图现在可以正常构建 — 不再被 detect_cycle 拦截。
#[test]
fn test_cyclic_graph_allowed() {
    let result = build_graph("cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "a");
        let _ = g.end("b");
        Ok(())
    });

    assert!(result.is_ok(), "cyclic graph should be allowed to build");
}

/// 有环图执行时，max_steps 熔断器防止无限循环。
#[tokio::test]
async fn test_cyclic_graph_steps_exceeded() {
    let graph = build_graph("infinite_cycle", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.node("done", NodeKind::Task(TaskNode::new("done", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "a");
        let _ = g.end("done");
        Ok(())
    })
    .expect("cyclic graph should build");

    let executor = GraphExecutor::new(5);
    let result = executor.execute(Arc::new(graph), HashMap::new()).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::Terminal(TerminalError::StepsExceeded { limit }) => assert_eq!(limit, 5),
        other => panic!("expected StepsExceeded, got: {other}"),
    }
}

/// 有环图 + edge_if 条件回跳 — 最核心的 Agent 编排模式。
#[tokio::test]
async fn test_cyclic_graph_with_edge_if_exit() {
    let graph = build_graph("cyclic_with_exit", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.edge_if("b", "a", |s: &State| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 3)
                .unwrap_or(true)
        });
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(3));
    assert_eq!(result.execution_log.len(), 7);
}

/// ConditionNode 回跳 — 复杂多路分支场景的语法糖。
#[tokio::test]
async fn test_condition_node_back_jump() {
    let graph = build_graph("cond_back_jump", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node(
            "route",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("route")
                    .branch("a", |s: &State| {
                        s.get("count")
                            .and_then(|v| v.as_u64())
                            .map(|c| c < 2)
                            .unwrap_or(true)
                    })
                    .branch("end", |_| true)
                    .build(),
            ),
        );
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.edge("a", "route");
        let _ = g.edge("route", "a");
        let _ = g.edge("route", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(2));
}

#[test]
fn test_missing_node() {
    let result = build_graph("missing", |g| {
        let _ = g.start("a");
        let _ = g.edge("a", "nonexistent");
        let _ = g.end("nonexistent");
        Ok(())
    });

    assert!(result.is_err());
}

#[test]
fn test_missing_start() {
    let result = build_graph("no_start", |g| {
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.end("a");
        Ok(())
    });

    assert!(result.is_err());
}

#[test]
fn test_missing_end() {
    let result = build_graph("no_end", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        Ok(())
    });

    assert!(result.is_err());
}

#[tokio::test]
async fn test_execution_log() {
    let graph = build_graph("log", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.end("b");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
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
        nodes: vec![Arc::new(TaskNode::new("increment", |state| {
            let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            state.insert("count".into(), serde_json::json!(count + 1));
            Ok(())
        }))],
        edges: vec![],
    };

    let loop_node = LoopNode::new(
        "counter",
        body,
        |s: &State| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 3)
                .unwrap_or(true)
        },
        10,
    );

    let graph = build_graph("loop_test", |g| {
        let _ = g.start("loop");
        let _ = g.node("loop", NodeKind::Loop(Box::new(loop_node)));
        let _ = g.end("loop");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(3));
}

/// LoopNode 超限 — 独立于全局 max_steps。
#[tokio::test]
async fn test_loop_node_limit_exceeded() {
    let body = SubGraph {
        nodes: vec![Arc::new(TaskNode::new("no_op", |_| Ok(())))],
        edges: vec![],
    };

    let loop_node = LoopNode::new("infinite", body, |_| true, 2);

    let graph = build_graph("loop_limit", |g| {
        let _ = g.start("loop");
        let _ = g.node("loop", NodeKind::Loop(Box::new(loop_node)));
        let _ = g.end("loop");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::Terminal(TerminalError::LoopLimitExceeded { limit }) => assert_eq!(limit, 2),
        other => panic!("expected LoopLimitExceeded, got: {other}"),
    }
}

// ─── BarrierNode 测试（Human-in-the-loop）────────────────────────

/// BarrierNode 在阻塞模式下必须报错。
#[tokio::test]
async fn test_barrier_blocked_mode_error() {
    let graph = build_graph("barrier_blocked", |g| {
        let _ = g.start("barrier");
        let _ = g.node("barrier", NodeKind::Barrier(BarrierNode::new("review")));
        let _ = g.end("barrier");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::Terminal(TerminalError::InvalidGraph(msg)) => {
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
    let graph = build_graph("approve_flow", |g| {
        let _ = g.start("barrier");
        let _ = g.node("barrier", NodeKind::Barrier(BarrierNode::new("review")));
        let _ = g.node("after", NodeKind::Task(TaskNode::new("after", |_| Ok(()))));
        let _ = g.edge("barrier", "after");
        let _ = g.end("after");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, handle) = GraphExecutor::default()
        .execute_stream(Arc::new(graph), HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting {
                node_name,
                barrier_id,
                ..
            } => {
                assert_eq!(node_name, "review");
                let _ = handle.decide(barrier_id, BarrierDecision::Approve).await;
            }
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }
}

/// BarrierNode — Reject 决策 + edge_if 回跳。
#[tokio::test]
async fn test_barrier_reject_with_back_jump() {
    let graph = build_graph("reject_flow", |g| {
        let _ = g.start("task");
        let _ = g.node(
            "task",
            NodeKind::Task(TaskNode::new("task", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node("review", NodeKind::Barrier(BarrierNode::new("review")));
        let _ = g.edge("task", "review");
        let _ = g.edge_if("review", "task", |s: &State| {
            s.get("review.reject_reason").is_some()
        });
        let _ = g.node("done", NodeKind::Task(TaskNode::new("done", |_| Ok(()))));
        let _ = g.edge("review", "done");
        let _ = g.end("done");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, handle) = GraphExecutor::default()
        .execute_stream(Arc::new(graph), HashMap::new());

    let mut reject_count = 0;
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting {
                node_name,
                barrier_id,
                ..
            } => {
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
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }
}

/// BarrierNode — Modify 决策，修改 State 后继续。
#[tokio::test]
async fn test_barrier_modify() {
    let graph = build_graph("modify_flow", |g| {
        let _ = g.start("barrier");
        let _ = g.node("barrier", NodeKind::Barrier(BarrierNode::new("input")));
        let _ = g.node("after", NodeKind::Task(TaskNode::new("after", |_| Ok(()))));
        let _ = g.edge("barrier", "after");
        let _ = g.end("after");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, handle) = GraphExecutor::default()
        .execute_stream(Arc::new(graph), HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting { barrier_id, .. } => {
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
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }
}

/// BarrierNode — 超时自动 Reject。
#[tokio::test]
async fn test_barrier_timeout() {
    let graph = build_graph("timeout_flow", |g| {
        let _ = g.start("barrier");
        let _ = g.node(
            "barrier",
            NodeKind::Barrier(
                BarrierNode::new("review")
                    .timeout(Duration::from_millis(100))
                    .default_action(BarrierDefaultAction::Reject),
            ),
        );
        let _ = g.node("after", NodeKind::Task(TaskNode::new("after", |_| Ok(()))));
        let _ = g.edge("barrier", "after");
        let _ = g.end("after");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, _handle) = GraphExecutor::default()
        .execute_stream(Arc::new(graph), HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting { .. } => {
                // 故意不发送决策 — 让 BarrierNode 超时
            }
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            GraphEvent::GraphError { ref error, .. } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }
}

/// BarrierNode — Reroute 决策，跳转到指定节点。
#[tokio::test]
async fn test_barrier_reroute() {
    let graph = build_graph("reroute_flow", |g| {
        let _ = g.start("barrier");
        let _ = g.node("barrier", NodeKind::Barrier(BarrierNode::new("route")));
        let _ = g.node(
            "path_a",
            NodeKind::Task(TaskNode::new("path_a", |state| {
                state.insert("path".into(), serde_json::json!("A"));
                Ok(())
            })),
        );
        let _ = g.node(
            "path_b",
            NodeKind::Task(TaskNode::new("path_b", |state| {
                state.insert("path".into(), serde_json::json!("B"));
                Ok(())
            })),
        );
        let _ = g.edge("barrier", "path_a");
        let _ = g.edge("barrier", "path_b");
        let _ = g.edge("path_a", "end");
        let _ = g.edge("path_b", "end");
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, handle) = GraphExecutor::default()
        .execute_stream(Arc::new(graph), HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting { barrier_id, .. } => {
                let _ = handle
                    .decide(
                        barrier_id,
                        BarrierDecision::Reroute {
                            target: "path_b".into(),
                        },
                    )
                    .await;
            }
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }
}

/// 双重 Barrier 顺序执行 — 验证 DecisionRegistry 不破坏正常流程。
#[tokio::test]
async fn test_double_barrier_sequential() {
    let graph = build_graph("double_barrier", |g| {
        let _ = g.start("before_a");
        let _ = g.node(
            "before_a",
            NodeKind::Task(TaskNode::new("before_a", |state| {
                state.insert("steps".into(), serde_json::json!(Vec::<String>::new()));
                Ok(())
            })),
        );
        let _ = g.node(
            "barrier_a",
            NodeKind::Barrier(BarrierNode::new("barrier_a")),
        );
        let _ = g.node(
            "between",
            NodeKind::Task(TaskNode::new("between", |state| {
                let mut steps: Vec<String> = state.get_json("steps").unwrap_or_default();
                steps.push("passed_a".into());
                state.set("steps", steps);
                Ok(())
            })),
        );
        let _ = g.node(
            "barrier_b",
            NodeKind::Barrier(BarrierNode::new("barrier_b")),
        );
        let _ = g.node(
            "after_b",
            NodeKind::Task(TaskNode::new("after_b", |state| {
                let mut steps: Vec<String> = state.get_json("steps").unwrap_or_default();
                steps.push("passed_b".into());
                state.set("steps", steps);
                Ok(())
            })),
        );
        let _ = g.edge("before_a", "barrier_a");
        let _ = g.edge("barrier_a", "between");
        let _ = g.edge("between", "barrier_b");
        let _ = g.edge("barrier_b", "after_b");
        let _ = g.end("after_b");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, handle) = GraphExecutor::default()
        .execute_stream(Arc::new(graph), HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting { barrier_id, .. } => {
                // 按顺序提交 Approve
                let _ = handle.decide(barrier_id, BarrierDecision::Approve).await;
            }
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected graph error: {error:?}");
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
    state.set("config", serde_json::json!({"nested": {"key": "value"}}));

    let config: serde_json::Value = state.get_json("config").unwrap();
    assert_eq!(config["nested"]["key"], "value");

    let err = state.get_json::<String>("missing");
    assert!(err.is_err());
}

#[test]
fn test_state_ext_append_array() {
    let mut state = State::new();
    state
        .append_array("items", serde_json::json!([1, 2]))
        .unwrap();
    state
        .append_array("items", serde_json::json!([3, 4]))
        .unwrap();

    let items = state.get("items").unwrap();
    assert_eq!(items, &serde_json::json!([1, 2, 3, 4]));
}

#[test]
fn test_state_ext_reduce() {
    let mut state = State::new();
    state.insert("items".into(), serde_json::json!([1, 2]));
    state
        .reduce("items", serde_json::json!([3, 4]), &array_reducer())
        .unwrap();

    let items = state.get("items").unwrap();
    assert_eq!(items, &serde_json::json!([1, 2, 3, 4]));
}

// ─── Edge Policy 测试 ──────────────────────────────────────────

/// 边级 policy — 超过 MaxVisits 返回 EdgePolicyExceeded。
#[tokio::test]
async fn test_edge_policy_exceeded() {
    let graph = build_graph("edge_policy", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.edge("a", "b");
        // 条件边 + policy MaxVisits=2：最多回跳 2 次
        let _ = g.edge_if("b", "a", |s: &State| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 10)
                .unwrap_or(true)
        });
        let _ = g.edge_policy(
            "b",
            "a",
            EdgePolicy::MaxVisits {
                limit: 2,
                on_exceeded: EdgeExceededStrategy::Strict,
            },
        );
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::Terminal(TerminalError::EdgePolicyExceeded { edge, limit }) => {
            assert_eq!(edge, "b→a");
            assert_eq!(limit, 2);
        }
        other => panic!("expected EdgePolicyExceeded, got: {other}"),
    }
}

/// 边级 analysis max_visits 仅用于静态分析，不参与 runtime — 正常退出。
#[tokio::test]
async fn test_edge_analysis_no_runtime_interference() {
    let graph = build_graph("edge_analysis_ok", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.edge("a", "b");
        // analysis max_visits 不参与 runtime，仅用于 analyze_cycles()
        let _ = g.edge_analysis("b", "a", 5);
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    // 图有环，但 analyze_cycles 应显示已保护
    let analysis = graph.analyze_cycles();
    assert!(analysis.has_cycles);
    assert!(analysis.all_protected());
}

// ─── Cycle Analysis 测试 ────────────────────────────────────────

/// DAG 无环。
#[test]
fn test_analyze_cycles_dag() {
    let graph = build_graph("dag", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.end("b");
        Ok(())
    })
    .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    assert!(!analysis.has_cycles);
    assert!(analysis.cycles.is_empty());
    assert!(analysis.all_protected());
}

/// 有环图检测到环。
#[test]
fn test_analyze_cycles_detected() {
    let graph = build_graph("cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.node("c", NodeKind::Task(TaskNode::new("c", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "c");
        let _ = g.edge("c", "a");
        let _ = g.end("a");
        Ok(())
    })
    .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    assert!(analysis.has_cycles);
    assert!(!analysis.cycles.is_empty());
}

/// 有环图 + analysis max_visits 保护。
#[test]
fn test_analyze_cycles_protected() {
    let graph = build_graph("protected_cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.edge_analysis("b", "a", 5);
        let _ = g.edge("b", "end");
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    assert!(analysis.has_cycles);
    assert!(analysis.all_protected());
}

/// analyze_cycles 生成诊断报告。
#[test]
fn test_analyze_cycles_report() {
    let graph = build_graph("report_test", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "a");
        let _ = g.end("a");
        Ok(())
    })
    .expect("build should succeed");

    let analysis = graph.analyze_cycles();
    let report = analysis.report();
    assert!(report.contains("Cycle Analysis"));
    assert!(report.contains("cycle"));
}

// ─── TraceId / SpanId 测试 ──────────────────────────────────────

/// TraceId 生成唯一 ID。
#[test]
fn test_trace_id_uniqueness() {
    let id1 = TraceId::new();
    let id2 = TraceId::new();
    assert_ne!(id1.to_string(), id2.to_string());
}

/// 流式执行事件包含 span_id。
#[tokio::test]
async fn test_stream_has_span_id() {
    let graph = build_graph("trace_test", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))));
        let _ = g.edge("a", "b");
        let _ = g.end("b");
        Ok(())
    })
    .expect("build should succeed");

    let (mut stream, _handle) =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

    let mut span_ids = Vec::new();
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::NodeStart { span_id, .. } => {
                span_ids.push(span_id);
            }
            GraphEvent::NodeEnd { span_id, .. } => {
                span_ids.push(span_id);
            }
            GraphEvent::GraphComplete { .. } => {
                break;
            }
            _ => {}
        }
    }

    // 至少有两个节点，每个有 start + end，共 4 个 span_id
    assert!(span_ids.len() >= 4);
}

// ─── Goto 边校验 + Policy 测试 ─────────────────────────────────

/// ConditionNode 返回 Goto(target) 的回跳边，analysis max_visits 用于静态检查。
/// Goto 跳转通过 transition() 校验边存在 + 记录访问计数。
#[tokio::test]
async fn test_goto_edge_with_analysis() {
    let graph = build_graph("goto_analysis", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                state.insert("count".into(), serde_json::json!(count + 1));
                Ok(())
            })),
        );
        let _ = g.node(
            "route",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("route")
                    .branch("a", |s: &State| {
                        s.get("count")
                            .and_then(|v| v.as_u64())
                            .map(|c| c < 2)
                            .unwrap_or(true)
                    })
                    .branch("end", |_| true)
                    .build(),
            ),
        );
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.edge("a", "route");
        // route → a 是 ConditionNode 的 Goto 目标
        let _ = g.edge_if("route", "a", |_| true);
        let _ = g.edge("route", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(2));
}

/// Goto(target) 但图中没有对应的边 → MissingEdge 错误。
#[tokio::test]
async fn test_goto_missing_edge_error() {
    let graph = build_graph("missing_edge", |g| {
        let _ = g.start("route");
        let _ = g.node(
            "route",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("route")
                    .branch("nonexistent", |_| true)
                    .build(),
            ),
        );
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))));
        let _ = g.edge("route", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::Terminal(TerminalError::MissingEdge { from, to }) => {
            assert_eq!(from, "route");
            assert_eq!(to, "nonexistent");
        }
        other => panic!("expected MissingEdge, got: {other}"),
    }
}
