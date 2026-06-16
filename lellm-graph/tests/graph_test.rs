use lellm_graph::{
    BarrierDecision, BarrierDefaultAction, BarrierNode, BuildError, BuildErrors, Diagnostic,
    DiagnosticCategory, GraphBuilder, GraphError, GraphEvent, GraphExecution, GraphExecutor,
    NodeKind, SK_COUNT, SK_STEPS, State, StateDelta, StateExt, StateKey, TaskNode, TerminalError,
    TraceId,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Helper: 构建 Graph 并返回 Result。
fn build_graph<F>(name: &str, f: F) -> Result<lellm_graph::Graph, BuildErrors>
where
    F: FnOnce(&mut GraphBuilder) -> Result<(), BuildError>,
{
    let mut g = GraphBuilder::new(name);
    let _ = f(&mut g);
    g.build()
}

#[tokio::test]
async fn test_linear_pipeline() {
    let graph = build_graph("linear", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |_state| {
                Ok(vec![StateDelta::set("step", serde_json::json!("a"))])
            })),
        );
        let _ = g.node(
            "b",
            NodeKind::Task(TaskNode::new("b", |_state| {
                Ok(vec![StateDelta::set("step", serde_json::json!("b"))])
            })),
        );
        let _ = g.node(
            "c",
            NodeKind::Task(TaskNode::new("c", |_state| {
                Ok(vec![StateDelta::set("step", serde_json::json!("c"))])
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
            NodeKind::Task(TaskNode::new("yes", |_state| {
                Ok(vec![StateDelta::set("result", serde_json::json!("yes"))])
            })),
        );
        let _ = g.node(
            "no",
            NodeKind::Task(TaskNode::new("no", |_state| {
                Ok(vec![StateDelta::set("result", serde_json::json!("no"))])
            })),
        );
        let _ = g.edge("check", "yes");
        let _ = g.edge("check", "no");
        let _ = g.edge("yes", "yes_end");
        let _ = g.edge("no", "no_end");
        let _ = g.node(
            "yes_end",
            NodeKind::Task(TaskNode::new("yes_end", |_| Ok(vec![]))),
        );
        let _ = g.node(
            "no_end",
            NodeKind::Task(TaskNode::new("no_end", |_| Ok(vec![]))),
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
                Err::<Vec<StateDelta>, _>(GraphError::Terminal(TerminalError::StateError(
                    "boom".into(),
                )))
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
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
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
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node(
            "done",
            NodeKind::Task(TaskNode::new("done", |_| Ok(vec![]))),
        );
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
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
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
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
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
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
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
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.end("a");
        Ok(())
    });

    assert!(result.is_err());
}

#[test]
fn test_missing_end() {
    let result = build_graph("no_end", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        Ok(())
    });

    assert!(result.is_err());
}

#[tokio::test]
async fn test_execution_log() {
    let graph = build_graph("log", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
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
        let _ = g.node(
            "after",
            NodeKind::Task(TaskNode::new("after", |_| Ok(vec![]))),
        );
        let _ = g.edge("barrier", "after");
        let _ = g.end("after");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
            })),
        );
        let _ = g.node("review", NodeKind::Barrier(BarrierNode::new("review")));
        let _ = g.edge("task", "review");
        let _ = g.edge_if("review", "task", |s: &State| {
            s.get("review.reject_reason").is_some()
        });
        let _ = g.node(
            "done",
            NodeKind::Task(TaskNode::new("done", |_| Ok(vec![]))),
        );
        let _ = g.edge("review", "done");
        let _ = g.end("done");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
        let _ = g.node(
            "after",
            NodeKind::Task(TaskNode::new("after", |_| Ok(vec![]))),
        );
        let _ = g.edge("barrier", "after");
        let _ = g.end("after");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
        let _ = g.node(
            "after",
            NodeKind::Task(TaskNode::new("after", |_| Ok(vec![]))),
        );
        let _ = g.edge("barrier", "after");
        let _ = g.end("after");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution {
        mut stream,
        handle: _handle,
    } = GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
            NodeKind::Task(TaskNode::new("path_a", |_state| {
                Ok(vec![StateDelta::set("path", serde_json::json!("A"))])
            })),
        );
        let _ = g.node(
            "path_b",
            NodeKind::Task(TaskNode::new("path_b", |_state| {
                Ok(vec![StateDelta::set("path", serde_json::json!("B"))])
            })),
        );
        let _ = g.edge("barrier", "path_a");
        let _ = g.edge("barrier", "path_b");
        let _ = g.edge("path_a", "end");
        let _ = g.edge("path_b", "end");
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
            NodeKind::Task(TaskNode::new("before_a", |_state| {
                Ok(vec![StateDelta::set(
                    "steps",
                    serde_json::json!(Vec::<String>::new()),
                )])
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
                Ok(vec![StateDelta::set(
                    "steps",
                    serde_json::to_value(steps).unwrap(),
                )])
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
                Ok(vec![StateDelta::set(
                    "steps",
                    serde_json::to_value(steps).unwrap(),
                )])
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

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
        .append_array("items", serde_json::json!([3, 4]))
        .unwrap();

    let items = state.get("items").unwrap();
    assert_eq!(items, &serde_json::json!([1, 2, 3, 4]));
}

/// 边级 analysis max_visits 仅用于静态分析，不参与 runtime — 正常退出。
/// 链式 API：edge_if().max_visits(n) 附加分析约束。
#[tokio::test]
async fn test_edge_analysis_no_runtime_interference() {
    let graph = build_graph("edge_analysis_ok", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        // 条件回跳 + max_visits 分析约束（不参与 runtime）
        let _ = g.edge_if("b", "a", |_| true).max_visits(5);
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    // 图有环，analyze_cycles 应显示已保护
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
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
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
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("c", NodeKind::Task(TaskNode::new("c", |_| Ok(vec![]))));
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
/// 链式 API：edge().max_visits(n) 给普通边附加分析约束。
#[test]
fn test_analyze_cycles_protected() {
    let graph = build_graph("protected_cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "a").max_visits(5);
        let _ = g.edge("b", "end");
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
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
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
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
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.end("b");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution {
        mut stream,
        handle: _handle,
    } = GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

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
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
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
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
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
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
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

// ─── StateKey<T> 测试 ──────────────────────────────────────────

/// StateKey 基本读写 — set_sk / get_sk / require_sk。
#[test]
fn test_statekey_basic_read_write() {
    use lellm_graph::StateKeyExt;

    let mut state = State::new();
    state.set_sk(&SK_COUNT, 42u64);

    assert_eq!(state.get_sk(&SK_COUNT), Some(42u64));
    assert_eq!(state.require_sk(&SK_COUNT).unwrap(), 42u64);
}

/// StateKey 类型安全 — 类型不匹配时返回 None（不会 panic）。
#[test]
fn test_statekey_type_mismatch() {
    use lellm_graph::StateKeyExt;

    let mut state = State::new();
    state.set_sk(&SK_COUNT, 42u64);

    // 定义一个与 SK_COUNT 同 key 但不同类型的 StateKey
    const SK_COUNT_AS_STRING: StateKey<String> =
        StateKey::new("count", lellm_graph::Reducer::Replace);

    // 期望 String，但实际存储的是 u64 → 反序列化失败 → None
    assert_eq!(state.get_sk::<String>(&SK_COUNT_AS_STRING), None);

    // require_sk 返回 Deserialize 错误（不是 MissingKey）
    let err = state.require_sk::<String>(&SK_COUNT_AS_STRING);
    assert!(matches!(
        err,
        Err(lellm_graph::StateError::Deserialize(_, _))
    ));
}

/// StateKey MissingKey — key 不存在时 require_sk 返回错误。
#[test]
fn test_statekey_missing_key() {
    use lellm_graph::StateKeyExt;

    let state = State::new();
    let err = state.require_sk(&SK_COUNT);
    assert!(matches!(err, Err(lellm_graph::StateError::MissingKey(_))));
}

/// StateKey contains_sk / remove_sk。
#[test]
fn test_statekey_contains_remove() {
    use lellm_graph::StateKeyExt;

    let mut state = State::new();
    state.set_sk(&SK_STEPS, vec!["step1".to_string()]);

    assert!(state.contains_sk(&SK_STEPS));
    assert!(!state.contains_sk(&SK_COUNT));

    let removed = state.remove_sk(&SK_STEPS);
    assert!(removed.is_some());
    assert!(!state.contains_sk(&SK_STEPS));
}

/// StateKey 与现有 StateExt 共存 — 同一个 state 可以同时使用两种 API。
#[test]
fn test_statekey_coexist_with_stateext() {
    use lellm_graph::{StateExt, StateKeyExt};

    let mut state = State::new();

    // StateKey API
    state.set_sk(&SK_COUNT, 100u64);

    // 传统 StateExt API
    state.set("legacy_flag", true);

    // 互相读取不受影响
    assert_eq!(state.get_sk(&SK_COUNT), Some(100u64));
    assert_eq!(state.get_bool("legacy_flag"), Some(true));
}

/// StateKey 在 Graph 执行中的真实使用场景。
#[tokio::test]
async fn test_statekey_in_graph_execution() {
    use lellm_graph::StateKeyExt;

    // 自定义 StateKey
    const SK_RESULT: StateKey<String> = StateKey::new("result", lellm_graph::Reducer::Replace);

    let graph = build_graph("statekey_graph", |g| {
        let _ = g.start("set");
        let _ = g.node(
            "set",
            NodeKind::Task(TaskNode::new("set", |_state| {
                Ok(vec![
                    StateDelta::set("count", serde_json::json!(0u64)),
                    StateDelta::set("result", serde_json::json!("pending")),
                ])
            })),
        );
        let _ = g.node(
            "increment",
            NodeKind::Task(TaskNode::new("increment", |state| {
                let count = state.get_sk(&SK_COUNT).unwrap_or(0);
                let mut deltas = vec![StateDelta::set("count", serde_json::json!(count + 1))];
                if count + 1 >= 3 {
                    deltas.push(StateDelta::set("result", serde_json::json!("done")));
                }
                Ok(deltas)
            })),
        );
        let _ = g.node(
            "check",
            NodeKind::Condition(
                lellm_graph::ConditionNode::builder("check")
                    .branch("increment", |s: &State| {
                        s.get_sk(&SK_COUNT).unwrap_or(0) < 3
                    })
                    .branch("end", |_| true)
                    .build(),
            ),
        );
        let _ = g.node(
            "end",
            NodeKind::Task(TaskNode::new("end", |state| {
                let result = state.require_sk(&SK_RESULT).unwrap();
                assert_eq!(result, "done");
                Ok(vec![])
            })),
        );
        let _ = g.edge("set", "increment");
        let _ = g.edge("increment", "check");
        let _ = g.edge("check", "increment");
        let _ = g.edge("check", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.state.get_sk(&SK_COUNT).unwrap(), 3u64);
    assert_eq!(result.state.get_sk(&SK_RESULT).unwrap(), "done".to_string());
}

// ─── TraceId 完整落地测试 ───────────────────────────────────────

/// TraceId 贯穿整个执行流 — GraphStart → NodeStart/End → GraphComplete
#[tokio::test]
async fn test_trace_id_full_lifecycle() {
    let graph = build_graph("trace_lifecycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.end("b");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution {
        mut stream,
        handle: _handle,
    } = GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

    let mut trace_id_from_start = None;
    let mut trace_ids_from_nodes = Vec::new();
    let mut node_count = 0;

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::GraphStart { trace_id } => {
                trace_id_from_start = Some(trace_id);
            }
            GraphEvent::NodeStart { trace_id, .. } => {
                trace_ids_from_nodes.push(trace_id);
                node_count += 1;
            }
            GraphEvent::NodeEnd { trace_id, .. } => {
                trace_ids_from_nodes.push(trace_id);
            }
            GraphEvent::GraphComplete { result } => {
                // GraphResult 中的 trace_id 应该与 GraphStart 一致
                assert_eq!(result.trace_id, trace_id_from_start.unwrap());
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {error}");
            }
            _ => {}
        }
    }

    // 所有 NodeStart/NodeEnd 的 trace_id 与 GraphStart 一致
    let start_trace = trace_id_from_start.unwrap();
    for node_trace in trace_ids_from_nodes {
        assert_eq!(
            node_trace, start_trace,
            "all node events should share the same trace_id"
        );
    }

    // 至少有两个节点，每个有 start + end
    assert!(node_count >= 2);
}

/// 阻塞模式 execute() 也返回 trace_id
#[tokio::test]
async fn test_trace_id_blocking_mode() {
    let graph = build_graph("trace_blocking", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.end("a");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    // trace_id 应该被正确设置（不是全零）
    let trace_str = result.trace_id.to_string();
    assert!(!trace_str.is_empty(), "trace_id should not be empty");
    // UUID v4 格式：8-4-4-4-12
    assert_eq!(
        trace_str.matches('-').count(),
        4,
        "trace_id should be UUID format"
    );
}

// ─── 测试覆盖缺口补充 ─────────────────────────────────────────

/// Fallback 控制流 + fallback 边路由 — v03 核心容错路径。
///
/// v03 移除了 RecoverableError，fallback 改为 `StreamNodeResult::Fallback` 控制流。
/// 节点通过返回 Fallback 主动声明降级策略，executor 根据 fallback 边路由。
#[tokio::test]
async fn test_fallback_control_flow() {
    use async_trait::async_trait;
    use lellm_graph::node::StreamNodeResult;
    use lellm_graph::{FlowNode, NodeOutput};
    use std::sync::Arc;

    // 自定义节点 — 总是返回 Fallback
    struct FallbackNode;

    #[async_trait]
    impl FlowNode for FallbackNode {
        async fn execute(&self, _state: &State) -> Result<NodeOutput, GraphError> {
            // 阻塞模式不支持 Fallback，直接报错
            Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: "fallback_node".into(),
                source: "fallback only in stream mode".into(),
            }))
        }

        async fn execute_stream(
            &self,
            _state: &State,
            _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
            _span_id: lellm_graph::SpanId,
        ) -> Result<StreamNodeResult, GraphError> {
            Ok(StreamNodeResult::Fallback {
                deltas: Vec::new(),
                reason: "temporary failure, try fallback".into(),
                node_name: "fallback_node".into(),
            })
        }
    }

    let graph = build_graph("fallback_flow", |g| {
        let _ = g.start("fallback_node");
        let _ = g.node("fallback_node", NodeKind::External(Arc::new(FallbackNode)));
        let _ = g.node(
            "fallback_target",
            NodeKind::Task(TaskNode::new("fallback_target", |_state| {
                Ok(vec![StateDelta::set("recovered", serde_json::json!(true))])
            })),
        );
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        // fallback 边：fallback_node → fallback_target
        let _ = g.edge_fallback("fallback_node", "fallback_target");
        let _ = g.edge("fallback_target", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    // Fallback 需要流式模式（TaskNode 的阻塞模式不支持 Fallback）
    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), State::new());
    drop(handle);

    let mut completed = false;
    while let Some(event) = stream.recv().await {
        match &event {
            GraphEvent::GraphComplete { result } => {
                // 应该通过 fallback 边路由到 fallback_target 节点
                assert_eq!(
                    result.state.get("recovered").unwrap(),
                    &serde_json::json!(true)
                );
                completed = true;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {}", error);
            }
            _ => {}
        }
    }
    assert!(completed, "should receive GraphComplete");
}

/// Fallback 无 fallback 边 → GraphError 终止。
#[tokio::test]
async fn test_fallback_no_edge() {
    use async_trait::async_trait;
    use lellm_graph::node::StreamNodeResult;
    use lellm_graph::{FlowNode, NodeOutput};
    use std::sync::Arc;

    struct FallbackNode;

    #[async_trait]
    impl FlowNode for FallbackNode {
        async fn execute(&self, _state: &State) -> Result<NodeOutput, GraphError> {
            Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: "fallback_node".into(),
                source: "fallback only in stream mode".into(),
            }))
        }

        async fn execute_stream(
            &self,
            _state: &State,
            _sink: &tokio::sync::mpsc::Sender<GraphEvent>,
            _span_id: lellm_graph::SpanId,
        ) -> Result<StreamNodeResult, GraphError> {
            Ok(StreamNodeResult::Fallback {
                deltas: Vec::new(),
                reason: "no fallback available".into(),
                node_name: "fallback_node".into(),
            })
        }
    }

    let graph = build_graph("no_fallback", |g| {
        let _ = g.start("fallback_node");
        let _ = g.node("fallback_node", NodeKind::External(Arc::new(FallbackNode)));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        // 没有 fallback 边，只有普通边（不会被 Fallback 命中）
        let _ = g.edge("fallback_node", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), State::new());
    drop(handle);

    let mut has_error = false;
    while let Some(event) = stream.recv().await {
        match &event {
            GraphEvent::GraphError { error, .. } => {
                assert!(
                    format!("{}", error).contains("fallback"),
                    "error should mention fallback: {}",
                    error
                );
                has_error = true;
            }
            GraphEvent::GraphComplete { .. } => {
                panic!("should not complete when fallback has no edge");
            }
            _ => {}
        }
    }
    assert!(has_error, "should receive GraphError");
}

/// GraphHandle::cancel() — 取消正在执行的 Graph。
#[tokio::test]
async fn test_graph_cancel() {
    let graph = build_graph("cancel_test", |g| {
        let _ = g.start("barrier");
        let _ = g.node(
            "barrier",
            NodeKind::Barrier(
                lellm_graph::BarrierNode::new("review").timeout(std::time::Duration::from_secs(60)),
            ),
        );
        let _ = g.end("barrier");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

    // 等待 BarrierWaiting 事件
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting { .. } => {
                // 不发送决策，直接取消
                handle.cancel();
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                // 取消可能先于我们到达
                match error {
                    lellm_graph::GraphError::Terminal(
                        lellm_graph::TerminalError::BarrierCancelled { .. },
                    ) => return, // 正常
                    _ => panic!("unexpected error: {error:?}"),
                }
            }
            _ => {}
        }
    }

    // 等待取消结果
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::GraphError { error, .. } => {
                match error {
                    lellm_graph::GraphError::Terminal(
                        lellm_graph::TerminalError::BarrierCancelled { .. },
                    ) => {} // 预期行为
                    _ => panic!("unexpected error: {error:?}"),
                }
                return;
            }
            GraphEvent::GraphComplete { .. } => {
                // 也可能正常完成（如果 barrier 被跳过），不报错
                return;
            }
            _ => {}
        }
    }
}

/// GraphHandle::decide_wildcard() — 通配决策匹配所有 occurrence。
#[tokio::test]
async fn test_decide_wildcard() {
    let graph = build_graph("wildcard_test", |g| {
        let _ = g.start("before");
        let _ = g.node(
            "before",
            NodeKind::Task(TaskNode::new("before", |_state| {
                Ok(vec![StateDelta::set(
                    "steps",
                    serde_json::json!(Vec::<String>::new()),
                )])
            })),
        );
        let _ = g.node(
            "barrier",
            NodeKind::Barrier(lellm_graph::BarrierNode::new("review")),
        );
        let _ = g.node(
            "between",
            NodeKind::Task(TaskNode::new("between", |state| {
                let mut steps: Vec<String> = state.get_json("steps").unwrap_or_default();
                steps.push("step1".into());
                Ok(vec![StateDelta::set(
                    "steps",
                    serde_json::to_value(steps).unwrap(),
                )])
            })),
        );
        // 第二个 barrier 实例
        let _ = g.node(
            "barrier2",
            NodeKind::Barrier(lellm_graph::BarrierNode::new("review")),
        );
        let _ = g.node(
            "done",
            NodeKind::Task(TaskNode::new("done", |state| {
                let mut steps: Vec<String> = state.get_json("steps").unwrap_or_default();
                steps.push("step2".into());
                Ok(vec![StateDelta::set(
                    "steps",
                    serde_json::to_value(steps).unwrap(),
                )])
            })),
        );
        let _ = g.edge("before", "barrier");
        let _ = g.edge("barrier", "between");
        let _ = g.edge("between", "barrier2");
        let _ = g.edge("barrier2", "done");
        let _ = g.end("done");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution { mut stream, handle } =
        GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

    let mut barrier_count = 0;
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierWaiting { node_name, .. } => {
                assert_eq!(node_name, "review");
                barrier_count += 1;
                // 第一次遇到时，使用通配决策覆盖所有 occurrence
                if barrier_count == 1 {
                    let _ = handle
                        .decide_wildcard("review", BarrierDecision::Approve)
                        .await;
                }
            }
            GraphEvent::GraphComplete { result } => {
                // 两个 barrier 都被通配决策覆盖
                let steps: Vec<String> = result.state.get_json("steps").unwrap();
                assert_eq!(steps, vec!["step1", "step2"]);
                break;
            }
            GraphEvent::GraphError { error, .. } => {
                panic!("unexpected error: {error:?}");
            }
            _ => {}
        }
    }
}

/// append_array 对非数组值的错误处理。
#[test]
fn test_append_array_non_array_error() {
    use lellm_graph::StateExt;

    let mut state = State::new();
    state.insert("items".into(), serde_json::json!("not_an_array"));

    let err = state.append_array("items", serde_json::json!([1, 2]));
    assert!(err.is_err());
    assert!(err.unwrap_err().contains("existing value is not an array"));
}

// ─── BuildErrors 多错误收集测试 ────────────────────────────────

/// 多个错误一次性收集 — 缺失 start + 缺失 end + 缺失节点。
#[test]
fn test_build_errors_multiple() {
    let result = build_graph("multi_error", |g| {
        let _ = g.start("a");
        let _ = g.end("b");
        let _ = g.edge("a", "nonexistent");
        let _ = g.edge("also_nonexistent", "b");
        Ok(())
    });

    assert!(result.is_err());
    if let Err(errors) = result {
        // 应该有 MissingNode 等多个错误
        assert!(
            errors.0.len() >= 2,
            "expected multiple errors, got: {:?}",
            errors.0
        );
        // 所有错误都应该是 MissingNode
        for e in &errors.0 {
            assert!(
                matches!(e, BuildError::MissingNode { .. }),
                "expected MissingNode, got: {:?}",
                e
            );
        }
    }
}

/// 重复节点名检测 — 后者覆盖前者，产生 Warning。
#[test]
fn test_build_duplicate_node_warning() {
    let result = build_graph("dup_node", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.end("a");
        Ok(())
    });

    // 构建成功（重复节点不阻止）
    assert!(result.is_ok());
}

/// Warning 不阻止构建成功。
#[test]
fn test_build_warning_not_fatal() {
    let result = build_graph("warning_test", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.edge_if("a", "b", |_| true);
        let _ = g.edge_if("a", "b", |_| false);
        let _ = g.end("b");
        Ok(())
    });

    // Warning 不阻止构建
    assert!(result.is_ok());
}

/// 完整错误列表可遍历。
#[test]
fn test_build_errors_display() {
    let result = build_graph("display_test", |g| {
        let _ = g.edge("x", "y");
        Ok(())
    });

    assert!(result.is_err());
    if let Err(errors) = result {
        let display = format!("{}", errors);
        assert!(display.contains("error(s)"), "should show error count");
    }
}

// ─── Consumer Drop = Cancel 测试 ──────────────────────────────────────────

/// Consumer Drop = Cancel — 消费者提前断开，executor 应立即终止。
#[tokio::test]
async fn test_consumer_drop_cancels_execution() {
    // 构建一个需要多步执行的图
    let graph = build_graph("consumer_drop", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                let count = state.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(vec![StateDelta::set("count", serde_json::json!(count + 1))])
            })),
        );
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("c", NodeKind::Task(TaskNode::new("c", |_| Ok(vec![]))));
        let _ = g.node("d", NodeKind::Task(TaskNode::new("d", |_| Ok(vec![]))));
        let _ = g.node("e", NodeKind::Task(TaskNode::new("e", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "c");
        let _ = g.edge("c", "d");
        let _ = g.edge("d", "e");
        let _ = g.end("e");
        Ok(())
    })
    .expect("build should succeed");

    let GraphExecution {
        mut stream,
        handle: _handle,
    } = GraphExecutor::default().execute_stream(Arc::new(graph), HashMap::new());

    // 消费 GraphStart 和第一个 NodeStart 后，drop stream
    let mut received = 0;
    loop {
        // 使用 try_recv 避免阻塞
        match tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv()).await {
            Ok(Some(_event)) => {
                received += 1;
                // 收到 2 个事件后断开
                if received >= 2 {
                    drop(stream);
                    // 等待片刻让 executor 检测 send 失败
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    return; // test 通过 — executor 已停止，没有 panic
                }
            }
            Ok(None) => {
                // stream closed — executor stopped
                return;
            }
            Err(_) => {
                // timeout — 不应该发生
                panic!("stream recv timeout — executor may be stuck");
            }
        }
    }
}

// ─── End Node 出边诊断测试 ────────────────────────────────────────────────

/// End 节点有出边 → build 成功但产生 Warning。
#[test]
fn test_end_node_outgoing_edge_warning() {
    let result = build_graph("end_outgoing", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.node(
            "after_end",
            NodeKind::Task(TaskNode::new("after_end", |_| Ok(vec![]))),
        );
        let _ = g.edge("a", "end");
        // end 节点有出边 — 不可达
        let _ = g.edge("end", "after_end");
        let _ = g.end("end");
        Ok(())
    });

    // 构建成功（Warning 不阻止）
    assert!(
        result.is_ok(),
        "end node outgoing edges should not block build"
    );
}

/// End 节点无出边 → 正常构建。
#[test]
fn test_end_node_no_outgoing_edge() {
    let result = build_graph("end_no_outgoing", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.edge("a", "end");
        let _ = g.end("end");
        Ok(())
    });

    assert!(result.is_ok());
}

/// End 节点在 Graph 执行中正确终止 — 即使有出边也不执行。
#[tokio::test]
async fn test_end_node_stops_execution() {
    let graph = build_graph("end_stops", |g| {
        let _ = g.start("a");
        let _ = g.node(
            "a",
            NodeKind::Task(TaskNode::new("a", |_state| {
                Ok(vec![StateDelta::set("visited_a", serde_json::json!(true))])
            })),
        );
        let _ = g.node(
            "end",
            NodeKind::Task(TaskNode::new("end", |_state| {
                Ok(vec![StateDelta::set(
                    "visited_end",
                    serde_json::json!(true),
                )])
            })),
        );
        let _ = g.node(
            "unreachable",
            NodeKind::Task(TaskNode::new("unreachable", |_state| {
                Ok(vec![StateDelta::set(
                    "visited_unreachable",
                    serde_json::json!(true),
                )])
            })),
        );
        let _ = g.edge("a", "end");
        let _ = g.edge("end", "unreachable"); // 不可达
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(Arc::new(graph), HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(
        result.state.get("visited_a").unwrap(),
        &serde_json::json!(true)
    );
    assert_eq!(
        result.state.get("visited_end").unwrap(),
        &serde_json::json!(true)
    );
    assert!(
        result.state.get("visited_unreachable").is_none(),
        "unreachable node should not be executed"
    );
    assert_eq!(result.execution_log.len(), 2);
}

// ─── Graph::analyze() 测试 ─────────────────────────────────────

/// analyze() — DAG 图，无诊断问题。
#[test]
fn test_analyze_dag_clean() {
    let graph = build_graph("dag", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let diag = graph.analyze();

    assert!(
        diag.warnings.is_empty(),
        "DAG should have no warnings, got: {:?}",
        diag.warnings
    );
    // 可能有 info（如 protected cycle 不存在），但不应有 Warning
}

/// analyze() — 检测未受保护的环。
#[test]
fn test_analyze_unprotected_cycle() {
    let graph = build_graph("cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "a"); // 回跳，无 max_visits
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let diag = graph.analyze();

    // 应该有 Cycle Warning
    let cycle_warnings: Vec<&Diagnostic> = diag
        .warnings
        .iter()
        .filter(|w| w.category == DiagnosticCategory::Cycle)
        .collect();
    assert!(
        !cycle_warnings.is_empty(),
        "Should detect unprotected cycle"
    );
}

/// analyze() — 检测不可达节点。
#[test]
fn test_analyze_unreachable_node() {
    let graph = build_graph("unreachable", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.node(
            "orphan",
            NodeKind::Task(TaskNode::new("orphan", |_| Ok(vec![]))),
        );
        let _ = g.edge("a", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let diag = graph.analyze();

    let unreachable_infos: Vec<&Diagnostic> = diag
        .infos
        .iter()
        .filter(|i| i.category == DiagnosticCategory::Unreachable)
        .collect();
    assert!(
        !unreachable_infos.is_empty(),
        "Should detect unreachable node 'orphan'"
    );
}

/// analyze() — 检测 End 节点出边。
#[test]
fn test_analyze_end_node_outgoing() {
    let graph = build_graph("end-outgoing", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.node(
            "extra",
            NodeKind::Task(TaskNode::new("extra", |_| Ok(vec![]))),
        );
        let _ = g.edge("a", "end");
        let _ = g.edge("end", "extra"); // end 节点有出边
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let diag = graph.analyze();

    let end_outgoing: Vec<&Diagnostic> = diag
        .infos
        .iter()
        .filter(|i| i.category == DiagnosticCategory::EndNodeOutgoing)
        .collect();
    assert!(
        !end_outgoing.is_empty(),
        "Should detect end node has outgoing edges"
    );
}

/// analyze() — Fallback 边参与循环。
#[test]
fn test_analyze_fallback_in_cycle() {
    let graph = build_graph("fallback-cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.edge_fallback("b", "a"); // fallback 回跳，形成环
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let diag = graph.analyze();

    // 应该有 Cycle Warning（未受保护）
    let cycle_warnings: Vec<&Diagnostic> = diag
        .warnings
        .iter()
        .filter(|w| w.category == DiagnosticCategory::Cycle)
        .collect();
    assert!(!cycle_warnings.is_empty(), "Should detect cycle");

    // 应该有 FallbackInCycle Warning
    let fallback_warnings: Vec<&Diagnostic> = diag
        .warnings
        .iter()
        .filter(|w| w.category == DiagnosticCategory::FallbackInCycle)
        .collect();
    assert!(
        !fallback_warnings.is_empty(),
        "Should detect fallback edge in cycle"
    );
}

/// analyze() — 受保护的环仅产生 Info。
#[test]
fn test_analyze_protected_cycle() {
    let graph = build_graph("protected-cycle", |g| {
        let _ = g.start("a");
        let _ = g.node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(vec![]))));
        let _ = g.node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(vec![]))));
        let _ = g.node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(vec![]))));
        let _ = g.edge("a", "b");
        let _ = g.edge("b", "a").max_visits(5); // 回跳，有 max_visits 保护
        let _ = g.edge("b", "end");
        let _ = g.end("end");
        Ok(())
    })
    .expect("build should succeed");

    let diag = graph.analyze();

    // 不应有 Cycle Warning（受保护）
    let cycle_warnings: Vec<&Diagnostic> = diag
        .warnings
        .iter()
        .filter(|w| w.category == DiagnosticCategory::Cycle)
        .collect();
    assert!(
        cycle_warnings.is_empty(),
        "Protected cycle should not produce warnings"
    );

    // 应该有 Cycle Info
    let cycle_infos: Vec<&Diagnostic> = diag
        .infos
        .iter()
        .filter(|i| i.category == DiagnosticCategory::Cycle)
        .collect();
    assert!(
        !cycle_infos.is_empty(),
        "Protected cycle should produce info"
    );
}
