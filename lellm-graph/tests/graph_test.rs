use lellm_graph::{
    BarrierDecision, BarrierDefaultAction, BarrierNode, GraphBuilder, GraphError, GraphEvent,
    GraphExecutor, LoopNode, NodeKind, SubGraph, TaskNode,
};
use std::collections::HashMap;
use std::time::Duration;

#[tokio::test]
async fn test_linear_pipeline() {
    let graph = GraphBuilder::new("linear")
        .start("a")
        .node(
            "a",
            NodeKind::Task(TaskNode::new("a", |state| {
                state.insert("step".into(), serde_json::json!("a"));
                Ok(())
            })),
        )
        .node(
            "b",
            NodeKind::Task(TaskNode::new("b", |state| {
                state.insert("step".into(), serde_json::json!("b"));
                Ok(())
            })),
        )
        .node(
            "c",
            NodeKind::Task(TaskNode::new("c", |state| {
                state.insert("step".into(), serde_json::json!("c"));
                Ok(())
            })),
        )
        .edge("a", "b")
        .edge("b", "c")
        .end("c")
        .build()
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
                    .branch("yes", |s| {
                        s.get("flag").and_then(|v| v.as_bool()).unwrap_or(false)
                    })
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

    // 路线 B：有环图构建成功，循环保护由 max_steps 提供
    assert!(result.is_ok(), "cyclic graph should be allowed to build");
}

/// 有环图执行时，max_steps 熔断器防止无限循环。
#[tokio::test]
async fn test_cyclic_graph_steps_exceeded() {
    // 构建一个无限循环的图：a -> b -> a -> b -> ...
    // end="done" 永远无法到达，max_steps 会熔断
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
        .edge("b", "a") // 无条件回跳，形成无限循环
        .end("done") // done 不可达
        .build()
        .expect("cyclic graph should build");

    // max_steps=5，循环会被熔断
    let executor = GraphExecutor::new(5);
    let result = executor.execute(&graph, HashMap::new()).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::StepsExceeded { limit } => assert_eq!(limit, 5),
        other => panic!("expected StepsExceeded, got: {other}"),
    }
}

/// 有环图 + edge_if 条件回跳 — 最核心的 Agent 编排模式。
///
/// 证明不需要 ConditionNode，纯 edge_if 即可实现回跳循环。
#[tokio::test]
async fn test_cyclic_graph_with_edge_if_exit() {
    // a -> b -> (count < 3 ? a : end)
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
        // 条件边：count < 3 时回跳 a（edge_if 天然支持回跳！）
        .edge_if("b", "a", |s| {
            s.get("count")
                .and_then(|v| v.as_u64())
                .map(|c| c < 3)
                .unwrap_or(true)
        })
        .edge("b", "end") // 无条件边作为 fallback
        .end("end")
        .build()
        .expect("build should succeed");

    let result = GraphExecutor::default()
        .execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    // a -> b -> a -> b -> a -> b -> end (3 次循环)
    assert_eq!(
        result.state.get("count").unwrap(),
        &serde_json::json!(3),
        "should complete exactly 3 iterations"
    );
    // execution_log: a, b, a, b, a, b, end = 7 entries
    assert_eq!(result.execution_log.len(), 7);
}

/// ConditionNode 回跳 — 复杂多路分支场景的语法糖。
#[tokio::test]
async fn test_condition_node_back_jump() {
    // ConditionNode 返回 Goto("a") 实现回跳
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

    // a -> route -> Goto(a) -> route -> Goto(end)
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

    // increment 3 次后，count=3，continue_condition 为 false，退出
    assert_eq!(result.state.get("count").unwrap(), &serde_json::json!(3));
}

/// LoopNode 超限 — 独立于全局 max_steps。
#[tokio::test]
async fn test_loop_node_limit_exceeded() {
    let body = SubGraph {
        nodes: vec![Box::new(TaskNode::new("no_op", |_| Ok(())))],
        edges: vec![],
    };

    // continue_condition 永远为 true，max_iterations=2
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

/// BarrierNode 流式模式 — Approve 决策。
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
    let mut stream = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { node_name, signal } => {
                assert_eq!(node_name, "review");
                let _ = signal.send(BarrierDecision::Approve);
            }
            GraphEvent::GraphComplete { result } => {
                // 审批标记应写入 State
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
        // 被拒绝则回跳 task，通过则前进到 done
        .edge_if("review", "task", |s| {
            s.get("review.reject_reason").is_some()
        })
        .node("done", NodeKind::Task(TaskNode::new("done", |_| Ok(()))))
        .edge("review", "done")
        .end("done")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let mut stream = GraphExecutor::default().execute_stream(graph, HashMap::new());

    let mut reject_count = 0;
    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { node_name, signal } => {
                assert_eq!(node_name, "review");
                reject_count += 1;
                if reject_count == 1 {
                    // 第一次拒绝
                    let _ = signal.send(BarrierDecision::Reject {
                        reason: "需要改进".into(),
                    });
                } else {
                    // 第二次通过
                    let _ = signal.send(BarrierDecision::Approve);
                }
            }
            GraphEvent::GraphComplete { result } => {
                // task 被执行了 2 次（初始 + 回跳）
                assert_eq!(
                    result.state.get("count").unwrap(),
                    &serde_json::json!(2),
                    "task should run twice: initial + after reject"
                );
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
    let mut stream = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { signal, .. } => {
                let _ = signal.send(BarrierDecision::Modify {
                    key: "user_input".into(),
                    value: serde_json::json!("人工补充的数据"),
                });
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
    let mut stream = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { signal, .. } => {
                // 故意不发送决策 — 但保持 sender 存活，
                // 让 BarrierNode 的 timeout 先触发
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    drop(signal);
                });
            }
            GraphEvent::GraphComplete { result } => {
                // 超时后自动 Reject，reject_reason 应写入 State
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
        .edge("barrier", "path_a") // 默认走 A
        .edge("path_a", "end")
        .edge("path_b", "end")
        .node("end", NodeKind::Task(TaskNode::new("end", |_| Ok(()))))
        .end("end")
        .build()
        .expect("build should succeed");

    let graph = std::sync::Arc::new(graph);
    let mut stream = GraphExecutor::default().execute_stream(graph, HashMap::new());

    loop {
        let event = stream.recv().await.expect("stream should not close");
        match event {
            GraphEvent::BarrierPaused { signal, .. } => {
                // 直接跳转到 path_b，跳过 path_a
                let _ = signal.send(BarrierDecision::Reroute {
                    target: "path_b".into(),
                });
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
