//! R4 新进程恢复测试辅助二进制。
//!
//! 模式：
//! - `run <dir> <effects_file> <block_node> <linear|loop>`
//! - `restore <dir> <trace_id> <max_steps> <effects_file> <linear|loop> [block_node]`
//!
//! 可靠握手：stdout 行 `HANDSHAKE <trace_id> <next_node>`（管道，不丢）+
//! 磁盘确认（直接查 store 文件，不依赖事件）。握手后无限阻塞，等待父进程 kill。
//!
//! **承诺边界**：验证「检查点成功落盘后进程被强制终止 → 新进程恢复不重跑
//! 已提交节点」。不承诺「工具成功但保存前崩溃」不重复执行（需幂等键/去重）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lellm_graph::{
    CheckpointConfig, FileBlobStore, GraphBuilder, NodeKind, SerdeCheckpointCodec, SimpleExecutor,
    State, StateExt, StateMutation, TaskNode, TraceId,
};
use uuid::Uuid;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(|s| s.as_str()) {
        Some("run") => tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(run(&args[1..])),
        Some("restore") => tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(restore(&args[1..])),
        _ => Err("usage: restore_probe <run|restore> ...".into()),
    };
    if let Err(e) = result {
        eprintln!("PROBE_ERROR {e}");
        std::process::exit(2);
    }
}

// ─── 图构建 ────────────────────────────────────────────────────

/// 普通效果节点 — 向 effects 文件追加自身名。
fn effect_fn(
    name: &str,
    effects: PathBuf,
) -> impl Fn(&mut lellm_graph::NodeContext) -> Result<(), lellm_graph::GraphError> + Send + Sync + 'static
{
    let name = name.to_string();
    move |_ctx| {
        effects_append(&effects, &name);
        Ok(())
    }
}

/// 阻塞节点 — 磁盘确认 → 握手 → 无限阻塞（防父进程 kill 前又执行下一节点）。
fn block_fn(
    block_node: String,
    dir: PathBuf,
) -> impl Fn(&mut lellm_graph::NodeContext) -> Result<(), lellm_graph::GraphError> + Send + Sync + 'static
{
    move |_ctx| {
        // 1. 找 trace 目录（dir 下唯一子目录）
        let trace_dir = std::fs::read_dir(&dir)
            .map_err(|e| io_err(e, "read dir"))?
            .into_iter()
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .map(|e| e.path())
            .ok_or_else(|| {
                lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(
                    "no trace dir".into(),
                ))
            })?;
        let trace_id_str = trace_dir
            .file_name()
            .expect("file_name")
            .to_string_lossy()
            .to_string();

        // 2. 磁盘确认：最大 seq 文件的 next_node == block_node（磁盘为真，不依赖事件）
        let files: Vec<(u64, PathBuf)> = std::fs::read_dir(&trace_dir)
            .map_err(|e| io_err(e, "read trace dir"))?
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                !n.ends_with(".tmp") && n.contains('_')
            })
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                let seq = n.split('_').next()?.parse::<u64>().ok()?;
                Some((seq, e.path()))
            })
            .collect();
        let (_, latest) = files.into_iter().max_by_key(|(s, _)| *s).ok_or_else(|| {
            lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(
                "no checkpoint on disk".into(),
            ))
        })?;
        let env: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&latest).map_err(|e| io_err(e, "read checkpoint"))?,
        )
        .map_err(|e| {
            lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(format!(
                "envelope: {e}"
            )))
        })?;
        let data = base64::engine::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            env["data"].as_str().expect("data str"),
        )
        .map_err(|e| {
            lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(format!(
                "base64: {e}"
            )))
        })?;
        let cp: serde_json::Value = serde_json::from_slice(&data).map_err(|e| {
            lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(format!(
                "checkpoint json: {e}"
            )))
        })?;
        let next = cp["next_node"].as_str().unwrap_or("");
        if next != block_node {
            return Err(lellm_graph::GraphError::Terminal(
                lellm_graph::TerminalError::StateError(format!(
                    "disk checkpoint next_node={next}, expected {block_node}"
                )),
            ));
        }

        // 3. 握手（stdout 管道，可靠）
        println!("HANDSHAKE {} {block_node}", trace_id_str);
        std::io::stdout().flush().map_err(|e| io_err(e, "flush"))?;

        // 4. 无限阻塞 — 等待父进程 kill
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }
}

fn io_err(e: std::io::Error, ctx: &str) -> lellm_graph::GraphError {
    lellm_graph::GraphError::Terminal(lellm_graph::TerminalError::StateError(format!(
        "{ctx}: {e}"
    )))
}

fn build_graph(
    kind: &str,
    effects: PathBuf,
    block_node: Option<&str>,
    dir: PathBuf,
) -> (lellm_graph::Graph, u64) {
    let g = match kind {
        "linear" => {
            let mk = |name: &str| {
                if Some(name) == block_node {
                    TaskNode::new(name, block_fn(name.to_string(), dir.clone()))
                } else {
                    TaskNode::new(name, effect_fn(name, effects.clone()))
                }
            };
            let mut b = GraphBuilder::<State>::new("probe_linear");
            b.start("a");
            b.node("a", NodeKind::Task(mk("a")));
            b.node("b", NodeKind::Task(mk("b")));
            b.node("c", NodeKind::Task(mk("c")));
            b.node("d", NodeKind::Task(mk("d")));
            b.edge("a", "b");
            b.edge("b", "c");
            b.edge("c", "d");
            b.end("d");
            b.build().expect("build linear")
        }
        "loop" => {
            let check_node = if block_node == Some("check") {
                TaskNode::new("check", block_fn("check".into(), dir.clone()))
            } else {
                TaskNode::new("check", effect_fn("check", effects.clone()))
            };
            let work_effects = effects.clone();
            let work_fn = move |ctx: &mut lellm_graph::NodeContext| {
                effects_append(&work_effects, "work");
                let count = ctx.state().get_i64("count").unwrap_or(0);
                ctx.record(StateMutation::Put(
                    "count".into(),
                    serde_json::json!(count + 1),
                ));
                Ok(())
            };
            let mut b = GraphBuilder::<State>::new("probe_loop");
            b.start("init");
            b.node(
                "init",
                NodeKind::Task(TaskNode::new("init", effect_fn("init", effects.clone()))),
            );
            b.node("check", NodeKind::Task(check_node));
            b.node("work", NodeKind::Task(TaskNode::new("work", work_fn)));
            b.node(
                "done",
                NodeKind::Task(TaskNode::new("done", effect_fn("done", effects.clone()))),
            );
            b.edge("init", "check");
            b.edge_if("check", "work", |s: &State| {
                s.get_i64("count").unwrap_or(0) < 3
            });
            b.edge_if("check", "done", |s: &State| {
                s.get_i64("count").unwrap_or(0) >= 3
            });
            b.edge("work", "check");
            b.end("done");
            b.build().expect("build loop")
        }
        other => panic!("unknown graph kind: {other}"),
    };
    let hash = g.canonical_hash();
    (g, hash)
}

fn effects_append(effects: &Path, name: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(effects)
        .expect("open effects");
    writeln!(f, "{name}").expect("append");
}

// ─── 模式 ──────────────────────────────────────────────────────

async fn run(args: &[String]) -> Result<(), String> {
    assert!(args.len() >= 4, "run <dir> <effects> <block_node> <kind>");
    let (dir, effects, block_node, kind) = (
        PathBuf::from(&args[0]),
        PathBuf::from(&args[1]),
        args[2].clone(),
        args[3].clone(),
    );
    let (graph, hash) = build_graph(&kind, effects, Some(&block_node), dir.clone());
    let store = Arc::new(FileBlobStore::new(dir));
    let config = CheckpointConfig::for_store(store, SerdeCheckpointCodec::<State>::new(), hash);
    let executor = SimpleExecutor::new(1000);
    let exec = executor
        .execute_stream_with_checkpoint(Arc::new(graph), State::new(), config)
        .map_err(|e| format!("entry: {e}"))?;
    // 消费事件直至终止（run 模式通常被 kill，不会到达）
    drain_events(exec.stream).await.map(|_| ())
}

async fn restore(args: &[String]) -> Result<(), String> {
    assert!(
        args.len() >= 5,
        "restore <dir> <trace_id> <max_steps> <effects> <kind> [block_node]"
    );
    let (dir, trace_id_str, max_steps, effects, kind) = (
        PathBuf::from(&args[0]),
        args[1].clone(),
        args[2].parse::<usize>().expect("max_steps"),
        PathBuf::from(&args[3]),
        args[4].clone(),
    );
    let block_node = args.get(5).cloned();
    let trace_id = TraceId(Uuid::parse_str(&trace_id_str).map_err(|e| e.to_string())?);

    let (graph, hash) = build_graph(&kind, effects, block_node.as_deref(), dir.clone());
    let store = Arc::new(FileBlobStore::new(dir));
    let codec = SerdeCheckpointCodec::<State>::new();
    let typed = lellm_graph::TypedCheckpointStore::new(&*store, codec.clone());
    let cp = typed
        .load_latest(&trace_id, hash)
        .await
        .map_err(|e| format!("load latest: {e}"))?
        .ok_or_else(|| "no checkpoint for trace".to_string())?;
    let config = CheckpointConfig::for_store(store, codec, hash);
    let executor = SimpleExecutor::new(max_steps);
    let exec = executor
        .execute_stream_with_restore(Arc::new(graph), cp, trace_id, config)
        .await
        .map_err(|e| format!("restore entry: {e}"))?;

    match drain_events(exec.stream).await {
        Ok(state) => {
            println!(
                "COMPLETE {}",
                serde_json::to_string(&state).expect("state json")
            );
            Ok(())
        }
        Err(msg) => {
            println!("ERROR {msg}");
            Err(msg)
        }
    }
}

/// 消费事件至终止。Ok(state) = GraphComplete；Err(msg) = GraphError/流意外关闭。
async fn drain_events(mut stream: lellm_graph::GraphStream) -> Result<State, String> {
    loop {
        match stream.recv().await {
            Some(lellm_graph::GraphEvent::GraphComplete { result }) => return Ok(result.state),
            Some(lellm_graph::GraphEvent::GraphError { error, .. }) => {
                return Err(error.to_string());
            }
            Some(_) => {}
            None => return Err("stream closed without terminal event".into()),
        }
    }
}
