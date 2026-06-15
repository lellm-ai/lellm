use lellm_graph::{GraphBuilder, GraphError, GraphExecutor, NodeKind, TaskNode};
use std::collections::HashMap;

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
    let result = GraphExecutor::execute(&graph, initial_state)
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
    let result = GraphExecutor::execute(&graph, initial_state)
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

    let result = GraphExecutor::execute(&graph, HashMap::new()).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        GraphError::StateError(msg) => assert_eq!(msg, "boom"),
        other => panic!("expected StateError, got: {other}"),
    }
}

#[test]
fn test_cycle_detection() {
    let result = GraphBuilder::new("cycle")
        .start("a")
        .node("a", NodeKind::Task(TaskNode::new("a", |_| Ok(()))))
        .node("b", NodeKind::Task(TaskNode::new("b", |_| Ok(()))))
        .edge("a", "b")
        .edge("b", "a")
        .end("b")
        .build();

    match result {
        Err(GraphError::InvalidGraph(msg)) => assert!(msg.contains("cycle")),
        Err(_) => panic!("expected InvalidGraph"),
        Ok(_) => panic!("expected error"),
    }
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

    let result = GraphExecutor::execute(&graph, HashMap::new())
        .await
        .expect("execution should succeed");

    assert_eq!(result.execution_log.len(), 2);
    assert!(result.execution_log.iter().all(|e| e.success));
    assert!(result.duration.as_nanos() > 0);
}
