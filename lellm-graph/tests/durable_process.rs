//! 真进程崩溃协议：磁盘确认后握手，父进程 kill，新进程获取锁并恢复。
//! 外部 IO 测试上限 30s；使用 stdout 握手，不通过 sleep 猜测进度。
//! 慢速 CI 若超过 1s，原因是测试进程启动及本机文件 IO。
use lellm_graph::{durable::*, *};
use std::{io::Write, sync::Arc};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

fn graph(path: std::path::PathBuf) -> Graph {
    let mut b = GraphBuilder::new("process-approval");
    b.start("review").end("effect");
    b.node("review", NodeKind::Barrier(BarrierNode::new("review")));
    b.node(
        "effect",
        NodeKind::Task(TaskNode::new("effect", move |_| {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "effect").unwrap();
            Ok(())
        })),
    );
    b.edge("review", "effect");
    b.build().unwrap()
}

#[tokio::test]
async fn durable_process_worker() {
    let Ok(dir) = std::env::var("LELLM_DURABLE_PROBE_DIR") else {
        return;
    };
    let phase = std::env::var("LELLM_DURABLE_PROBE_PHASE").unwrap();
    let rt = DurableRuntime::new(
        Arc::new(FileRunStore::new(&dir)),
        RunVersion::new("v1", "v1"),
    );
    let graph = graph(std::path::Path::new(&dir).join("effects"));
    if phase == "wait" {
        assert!(matches!(
            rt.start("run", &graph, State::new(), 2, CancellationToken::new())
                .await
                .unwrap(),
            RunOutcome::Suspended(_)
        ));
        // 再持锁后握手，验证强制终止会释放操作系统锁。
        let store = FileRunStore::new(&dir);
        let _lease = store.acquire("run").unwrap();
        println!("DURABLE_READY");
        std::io::stdout().flush().unwrap();
        std::future::pending::<()>().await;
    } else {
        let record = rt.inspect("run").unwrap();
        let request = record.approvals.values().next().unwrap();
        rt.submit_decision(
            "run",
            &request.id,
            &request.content_token,
            "approve",
            record.revision,
            ApprovalDecision::Approve,
        )
        .unwrap();
        assert!(matches!(
            rt.resume("run", &graph, CancellationToken::new())
                .await
                .unwrap(),
            RunOutcome::Completed(_)
        ));
        assert!(matches!(
            rt.resume("run", &graph, CancellationToken::new())
                .await
                .unwrap(),
            RunOutcome::Completed(_)
        ));
    }
}

#[tokio::test]
async fn approval_resumes_in_new_process_after_kill() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let spawn = |phase: &str| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "durable_process_worker", "--nocapture"])
                .env("LELLM_DURABLE_PROBE_DIR", dir.path())
                .env("LELLM_DURABLE_PROBE_PHASE", phase)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .unwrap()
        };
        let mut child = spawn("wait");
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        loop {
            if lines
                .next_line()
                .await
                .unwrap()
                .expect("worker closed before handshake")
                == "DURABLE_READY"
            {
                break;
            }
        }
        assert!(matches!(
            FileRunStore::new(dir.path()).acquire("run"),
            Err(DurableError::Busy)
        ));
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let status = spawn("resume").wait().await.unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("effects")).unwrap(),
            "effect\n"
        );
    })
    .await
    .expect("process recovery handshake timed out");
}

struct UncertainEffect(std::path::PathBuf);
#[async_trait::async_trait]
impl LeafNode<State> for UncertainEffect {
    async fn execute(&self, ctx: &mut LeafContext<'_, State>) -> Result<(), GraphError> {
        let result: i64 = TaskRuntime::current()
            .unwrap()
            .execute(
                "effect",
                "v1",
                serde_json::json!({}),
                ReplayPolicy::ManualRecovery,
                |_| async {
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&self.0)
                        .unwrap();
                    writeln!(file, "effect").unwrap();
                    file.flush().unwrap();
                    println!("EFFECT_READY");
                    std::io::stdout().flush().unwrap();
                    std::future::pending::<()>().await;
                    Ok(42)
                },
            )
            .await?;
        ctx.record(StateMutation::Put(
            "result".into(),
            serde_json::json!(result),
        ));
        Ok(())
    }
}
fn uncertain_graph(dir: &std::path::Path) -> Graph {
    let mut b = GraphBuilder::new("uncertain-effect");
    b.start("effect").end("effect");
    b.node(
        "effect",
        NodeKind::ExternalLeaf(Arc::new(UncertainEffect(dir.join("effects")))),
    );
    b.build().unwrap()
}
#[tokio::test]
async fn uncertain_process_worker() {
    let Ok(dir) = std::env::var("LELLM_UNCERTAIN_PROBE_DIR") else {
        return;
    };
    let graph = uncertain_graph(std::path::Path::new(&dir));
    let rt = DurableRuntime::new(
        Arc::new(FileRunStore::new(&dir)),
        RunVersion::new("v1", "v1"),
    );
    rt.start("run", &graph, State::new(), 1, CancellationToken::new())
        .await
        .unwrap();
}

// 若慢速 CI 超过 1s，耗时来自测试进程启动与文件 IO，不依赖计时等待。
#[tokio::test]
async fn external_success_before_receipt_requires_reconciliation_after_kill() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "uncertain_process_worker", "--nocapture"])
            .env("LELLM_UNCERTAIN_PROBE_DIR", dir.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        loop {
            if lines
                .next_line()
                .await
                .unwrap()
                .expect("worker closed before effect")
                == "EFFECT_READY"
            {
                break;
            }
        }
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let graph = uncertain_graph(dir.path());
        let rt = DurableRuntime::new(
            Arc::new(FileRunStore::new(dir.path())),
            RunVersion::new("v1", "v1"),
        );
        let RunOutcome::Suspended(record) = rt
            .resume("run", &graph, CancellationToken::new())
            .await
            .unwrap()
        else {
            panic!()
        };
        let task = record.tasks.values().next().unwrap();
        assert_eq!(task.status, TaskStatus::Unknown);
        rt.resolve_task(
            "run",
            &task.id,
            "external-confirmed",
            record.revision,
            TaskResolution::Completed(serde_json::json!(42)),
        )
        .unwrap();
        let RunOutcome::Completed(state) = rt
            .resume("run", &graph, CancellationToken::new())
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(state.get_i64("result"), Some(42));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("effects")).unwrap(),
            "effect\n"
        );
    })
    .await
    .expect("uncertain effect recovery timed out");
}
