//! 迁移示例 — 持久化执行 + 新进程恢复（format_version=1）。
//!
//! 运行：`cargo +1.88.0 run -p lellm-graph --example persistent_restore`
//!
//! 旧（System B，从未可用）：
//! ```rust,ignore
//! let session = ExecutionSession::new(state, graph);
//! let cp = session.checkpoint();
//! let restored = ExecutionSession::restore(cp, graph)?;
//! restored.run_with(&mut engine).await?;
//! ```
//!
//! 新：
//! ```rust,ignore
//! let store = Arc::new(FileBlobStore::new(dir));
//! let config = CheckpointConfig::for_store(store, SerdeCheckpointCodec::new(), graph.canonical_hash());
//! let exec = executor.execute_stream_with_checkpoint(graph, state, config)?;
//! // ... 进程崩溃后（新进程）...
//! let cp = typed.load_latest(&trace_id, graph.canonical_hash()).await?;
//! let exec = executor.execute_stream_with_restore(graph, cp, trace_id, config).await?;
//! ```

use std::sync::Arc;

use lellm_graph::{
    CheckpointConfig, FileBlobStore, GraphBuilder, GraphEvent, NodeKind, SerdeCheckpointCodec,
    SimpleExecutor, State, TaskNode, TypedCheckpointStore,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 唯一临时目录（示例不删除历史数据；OS 清理 /tmp）
    let dir = std::env::temp_dir().join(format!(
        "lellm_restore_example_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos()
    ));

    let mut b = GraphBuilder::<State>::new("demo");
    b.start("a");
    b.node(
        "a",
        NodeKind::Task(TaskNode::new("a", |_ctx| {
            println!("[node] a");
            Ok(())
        })),
    );
    b.node(
        "b",
        NodeKind::Task(TaskNode::new("b", |_ctx| {
            println!("[node] b");
            Ok(())
        })),
    );
    b.edge("a", "b");
    b.end("b");
    let graph = b.build()?;
    let hash = graph.canonical_hash();

    // 首次持久化执行
    let store = Arc::new(FileBlobStore::new(&dir));
    let config =
        CheckpointConfig::for_store(store.clone(), SerdeCheckpointCodec::<State>::new(), hash);
    let executor = SimpleExecutor::new(100);
    let mut exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph.clone()), State::new(), config.clone())
        .expect("entry");

    // 从 GraphStart 事件取 trace_id（新 trace）
    let mut trace_id = None;
    while let Some(ev) = exec.stream.recv().await {
        match ev {
            GraphEvent::GraphStart { trace_id: t } => trace_id = Some(t),
            GraphEvent::GraphComplete { .. } => break,
            GraphEvent::GraphError { error, .. } => return Err(error.to_string().into()),
            _ => {}
        }
    }
    let trace_id = trace_id.expect("GraphStart");
    println!("trace_id: {trace_id}");

    // 模拟「新进程」：加载最新检查点 → 恢复（同一 trace 继续保存）
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = TypedCheckpointStore::new(&*store, codec);
    let cp = typed
        .load_latest(&trace_id, hash)
        .await?
        .expect("checkpoint should exist");
    println!(
        "latest checkpoint: next_node={:?}, steps_used={}",
        cp.next_node, cp.steps_used
    );

    let mut exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, trace_id, config)
        .await
        .expect("restore validation");
    while let Some(ev) = exec.stream.recv().await {
        if let GraphEvent::GraphComplete { result } = ev {
            println!("restored state keys: {}", result.state.len());
            break;
        }
    }

    println!("checkpoints at: {}", dir.display());
    Ok(())
}
