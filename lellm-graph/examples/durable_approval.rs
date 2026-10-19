//! cargo run -p lellm-graph --example durable_approval
use lellm_graph::{durable::*, *};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = GraphBuilder::<State>::new("review-example");
    builder.start("review").end("apply");
    builder.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    builder.node(
        "apply",
        NodeKind::Task(TaskNode::new("apply", |ctx| {
            ctx.record(StateMutation::Put(
                "approved".into(),
                serde_json::json!(true),
            ));
            Ok(())
        })),
    );
    builder.edge("review", "apply");
    let graph = builder.build()?;
    // 替换为 FileRunStore::new(path) 即可跨进程读取同一 run_id。
    let runtime = DurableRuntime::new(
        Arc::new(MemoryRunStore::default()),
        RunVersion::new("review-v1", "state-v1"),
    );
    let RunOutcome::Suspended(record) = runtime
        .start("example", &graph, State::new(), 2, CancellationToken::new())
        .await?
    else {
        return Err("expected review".into());
    };
    let request = record.approvals.values().next().ok_or("missing review")?;
    // 实际应用在人工明确决定后提交此请求；保存确认后才返回给客户端。
    runtime.submit_decision(
        "example",
        &request.id,
        &request.content_token,
        "decision-1",
        record.revision,
        ApprovalDecision::Approve,
    )?;
    let RunOutcome::Completed(state) = runtime
        .resume("example", &graph, CancellationToken::new())
        .await?
    else {
        return Err("expected completion".into());
    };
    println!("{}", serde_json::to_string(&state.snapshot())?);
    Ok(())
}
