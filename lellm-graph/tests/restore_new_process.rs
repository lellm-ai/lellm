//! R4 新进程恢复集成测试 — 真崩溃协议。
//!
//! **承诺边界**：验证「检查点成功落盘后进程被强制终止（kill -9）→ 新进程恢复
//! 不重跑已提交节点」。**不能**证明「工具成功但保存前崩溃」不重复执行
//! （需幂等键/结果去重/业务补偿，暂缓项）。
//!
//! **可靠握手**：stdout 行（管道，不丢）+ 子进程磁盘确认。
//! `CheckpointSaved` 事件（try_send 可能丢）仅尽力观测，不作握手。
//!
//! **外部进程测试**（耗时 > 1s 原因：子进程启动 + 真崩溃协议）：
//! 总耗时 < 30s（握手超时 15s；正常路径 < 5s）。
//! 子进程握手后无限阻塞（防父进程 kill 前又执行下一节点）；
//! 父进程握手读取设超时，超时/失败也终止并回收子进程。

#[cfg(unix)]
mod unix {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncBufReadExt;

    fn probe() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_BIN_EXE_restore_probe"))
    }

    /// 读子进程 stdout 至 HANDSHAKE 行（超时/流结束/错误 → None）。
    async fn read_handshake(
        child: &mut tokio::process::Child,
        timeout: Duration,
    ) -> Option<String> {
        let stdout = child.stdout.as_mut()?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    if line.starts_with("HANDSHAKE ") {
                        return Some(line);
                    }
                }
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return None,
            }
        }
    }

    /// kill -9 + 回收，验证 signal 退出（R6.4）。
    async fn kill9_and_reap(child: &mut tokio::process::Child) -> Option<i32> {
        let _ = child.kill().await;
        match child.wait().await {
            Ok(status) => status.signal(),
            Err(_) => None,
        }
    }

    /// 失败路径：终止并回收子进程，返回 stderr（R6.4：任何失败路径都不得泄漏子进程）。
    async fn kill_and_collect_stderr(child: &mut tokio::process::Child) -> String {
        use tokio::io::AsyncReadExt;
        let stderr = child.stderr.take();
        let _ = child.kill().await;
        let _ = child.wait().await;
        match stderr {
            Some(mut se) => {
                let mut buf = Vec::new();
                match se.read_to_end(&mut buf).await {
                    Ok(_) => String::from_utf8_lossy(&buf).to_string(),
                    Err(e) => format!("read stderr failed: {e}"),
                }
            }
            None => String::new(),
        }
    }

    fn effects_lines(path: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .expect("read effects")
            .lines()
            .map(|s| s.to_string())
            .collect()
    }

    /// 启动子进程并等待握手；超时 → 终止回收 + panic（附 stderr）。
    async fn spawn_and_handshake(label: &str, args: &[&str]) -> (String, tokio::process::Child) {
        let mut child = tokio::process::Command::new(probe())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let line = match read_handshake(&mut child, Duration::from_secs(15)).await {
            Some(l) => l,
            None => panic!(
                "{label}: handshake timeout. stderr: {}",
                kill_and_collect_stderr(&mut child).await
            ),
        };
        let trace_id = line
            .split_whitespace()
            .nth(1)
            .expect("trace_id in HANDSHAKE line")
            .to_string();
        (trace_id, child)
    }

    /// T8: R4 主链路 — a→b→c→d，c 处握手后 kill -9，新进程恢复跑到完成
    #[tokio::test]
    async fn t8_r4_crash_kill9_restore_linear() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ckpt");
        let effects = tmp.path().join("effects.txt");

        // ① 子进程 A：run，block_node=c
        let (trace_id_str, mut child) = spawn_and_handshake(
            "A",
            &[
                "run",
                dir.to_str().unwrap(),
                effects.to_str().unwrap(),
                "c",
                "linear",
            ],
        )
        .await;
        assert_eq!(
            kill9_and_reap(&mut child).await,
            Some(9),
            "child A must exit by SIGKILL"
        );

        // 子进程 A 阶段断言：副作用 = [a, b]（c 未执行）；磁盘存在 trace 目录
        assert_eq!(effects_lines(&effects), vec!["a", "b"]);
        let trace_dir = dir.join(&trace_id_str);
        assert!(trace_dir.exists(), "trace dir on disk");

        // ② 子进程 B：restore 跑到完成
        let child_b = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "100",
                effects.to_str().unwrap(),
                "linear",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn B");
        let out_b = tokio::time::timeout(Duration::from_secs(15), child_b.wait_with_output())
            .await
            .expect("B timeout")
            .expect("reap B");
        assert!(
            out_b.status.success(),
            "B failed: stderr={}",
            String::from_utf8_lossy(&out_b.stderr)
        );
        let stdout_b = String::from_utf8_lossy(&out_b.stdout);
        assert!(stdout_b.contains("COMPLETE"), "B stdout: {stdout_b}");

        // 最终断言：副作用 = [a, b, c, d]（a/b 不重跑、c/d 各一次）
        assert_eq!(effects_lines(&effects), vec!["a", "b", "c", "d"]);
    }

    /// T9: 循环中途恢复 — 执行位置 + 步数预算延续（总预算到限才 StepsExceeded）
    #[tokio::test]
    async fn t9_crash_restore_loop_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ckpt");
        let effects = tmp.path().join("effects.txt");

        // 子进程 A：loop 图，block_node=check（首次到达，steps_used=1 后握手）
        let (trace_id_str, mut child) = spawn_and_handshake(
            "A",
            &[
                "run",
                dir.to_str().unwrap(),
                effects.to_str().unwrap(),
                "check",
                "loop",
            ],
        )
        .await;
        assert_eq!(kill9_and_reap(&mut child).await, Some(9));
        assert_eq!(effects_lines(&effects), vec!["init"]);

        // 子进程 B：restore，max_steps=5 → check(2) work(3) check(4) work(5) → step6 StepsExceeded
        let child_b = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "5",
                effects.to_str().unwrap(),
                "loop",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn B");
        let out_b = tokio::time::timeout(Duration::from_secs(15), child_b.wait_with_output())
            .await
            .expect("B timeout")
            .expect("reap B");
        let stdout_b = String::from_utf8_lossy(&out_b.stdout);
        assert!(
            stdout_b.contains("ERROR") && stdout_b.contains("step limit 5 exceeded"),
            "B stdout: {stdout_b}"
        );

        // 总执行 = 5 步（预算延续，非 1+5）；第 6 步（check）未执行
        assert_eq!(
            effects_lines(&effects),
            vec!["init", "check", "work", "check", "work"]
        );
    }

    /// T10: 双重恢复（R2 验收）— 保存 → 新进程恢复 → 再保存 → 再次新进程恢复
    #[tokio::test]
    async fn t10_double_restore_seq_continues() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("ckpt");
        let effects = tmp.path().join("effects.txt");

        // 子进程 A：run linear block c → 握手 → kill
        let (trace_id_str, mut child) = spawn_and_handshake(
            "A",
            &[
                "run",
                dir.to_str().unwrap(),
                effects.to_str().unwrap(),
                "c",
                "linear",
            ],
        )
        .await;
        assert_eq!(kill9_and_reap(&mut child).await, Some(9));
        assert_eq!(effects_lines(&effects), vec!["a", "b"]);

        // 子进程 B：restore（block_node=d）→ c 执行、保存 next=d、握手 → kill
        let (trace_id_b, mut child_b) = spawn_and_handshake(
            "B",
            &[
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "100",
                effects.to_str().unwrap(),
                "linear",
                "d",
            ],
        )
        .await;
        // B 延续同一 trace（seq 延续，未回退）
        assert_eq!(trace_id_b, trace_id_str);
        assert_eq!(kill9_and_reap(&mut child_b).await, Some(9));
        assert_eq!(effects_lines(&effects), vec!["a", "b", "c"]);

        // 子进程 C：restore 跑到完成
        let child_c = tokio::process::Command::new(probe())
            .args([
                "restore",
                dir.to_str().unwrap(),
                &trace_id_str,
                "100",
                effects.to_str().unwrap(),
                "linear",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn C");
        let out_c = tokio::time::timeout(Duration::from_secs(15), child_c.wait_with_output())
            .await
            .expect("C timeout")
            .expect("reap C");
        assert!(
            out_c.status.success(),
            "C failed: stderr={}",
            String::from_utf8_lossy(&out_c.stderr)
        );

        // 最终：[a, b, c, d] 各一次 — 若 seq 回退，C 会重跑 c，此处断言捕获
        assert_eq!(effects_lines(&effects), vec!["a", "b", "c", "d"]);
    }
}
