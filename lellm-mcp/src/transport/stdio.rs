//! Stdio Transport — 通过子进程 stdin/stdout 通信。
//!
//! 架构：
//! - connect() 启动子进程，spawn read_loop
//! - request() 通过 stdin 发送 JSON，等待 oneshot 响应
//! - notifications() 返回 notification stream

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot, watch};

use super::{ConnectionState, McpTransport, NotificationStream};
use crate::protocol::{
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpError,
};

/// 通知 channel 容量。
const NOTIFICATION_BUFFER: usize = 64;

/// 默认请求超时（秒）。
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Stdio Transport 配置。
#[derive(Debug, Clone)]
pub struct StdioConfig {
    /// 启动命令（如 "npx"）。
    pub command: String,
    /// 命令参数。
    pub args: Vec<String>,
    /// 环境变量（可选）。
    pub env: Option<Vec<(String, String)>>,
    /// 单次请求超时（默认 30 秒）。
    pub request_timeout: std::time::Duration,
}

impl StdioConfig {
    pub fn new(command: impl Into<String>, args: impl Into<Vec<String>>) -> Self {
        Self {
            command: command.into(),
            args: args.into(),
            env: None,
            request_timeout: std::time::Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        }
    }

    /// 设置环境变量。
    pub fn with_env(mut self, env: Option<Vec<(String, String)>>) -> Self {
        self.env = env;
        self
    }

    /// 设置请求超时。
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

/// Stdio Transport 实现。
pub struct StdioTransport {
    config: StdioConfig,
    inner: Option<Arc<StdioTransportInner>>,
    state: watch::Sender<ConnectionState>,
}

struct StdioTransportInner {
    #[allow(dead_code)]
    child: Child,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    notification_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
    next_id: std::sync::atomic::AtomicU64,
    shutdown: watch::Sender<bool>,
}

type PendingMap = HashMap<u64, oneshot::Sender<Result<JsonRpcResponse, McpError>>>;

impl StdioTransport {
    pub fn new(config: StdioConfig) -> Self {
        Self {
            config,
            inner: None,
            state: watch::Sender::new(ConnectionState::Disconnected),
        }
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn connect(&mut self) -> Result<(), McpError> {
        self.state.send(ConnectionState::Connecting).ok();

        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref envs) = self.config.env {
            for (key, value) in envs {
                cmd.env(key, value);
            }
        }

        let mut child = cmd.spawn().map_err(McpError::Io)?;

        let stdout = child.stdout.take().expect("stdout should be piped");
        let stdin = child.stdin.take().expect("stdin should be piped");
        let stderr = child.stderr.take().expect("stderr should be piped");

        // 后台读取 stderr，转为 tracing 日志
        let command_name = self.config.command.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            tracing::warn!(target: "mcp", command = %command_name, "{}", trimmed);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "mcp", command = %command_name, error = %e, "stderr read error");
                        break;
                    }
                }
            }
        });

        let (notification_tx, _) =
            tokio::sync::broadcast::channel::<JsonRpcNotification>(NOTIFICATION_BUFFER);
        let (shutdown_tx, _) = watch::channel(false);

        // 启动 read_loop
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        let notification_tx_for_loop = notification_tx.clone();
        tokio::spawn({
            let pending = Arc::clone(&pending);
            let mut shutdown = shutdown_tx.subscribe();
            async move {
                loop {
                    tokio::select! {
                        _ = shutdown.changed() => break,
                        result = lines.next_line() => {
                            let line = match result {
                                Ok(Some(line)) => line,
                                Ok(None) => break,
                                Err(e) => {
                                    tracing::warn!(error = %e, "stdio read error");
                                    break;
                                }
                            };

                            let msg: JsonRpcMessage = match serde_json::from_str(&line) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    tracing::warn!(error = %e, line, "invalid jsonrpc");
                                    continue;
                                }
                            };

                            match msg {
                                JsonRpcMessage::Response(resp) => {
                                    let mut p = pending.lock().await;
                                    if let Some(tx) = p.remove(&resp.id) {
                                        let _ = tx.send(Ok(resp));
                                    }
                                }
                                JsonRpcMessage::Notification(notif) => {
                                    let _ = notification_tx_for_loop.send(notif);
                                }
                                JsonRpcMessage::Request(_) => {
                                    tracing::warn!("unexpected request from server");
                                }
                            }
                        }
                    }
                }

                // read_loop 退出 → 清除所有 pending
                let mut p = pending.lock().await;
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err(McpError::Disconnected));
                }
            }
        });

        self.inner = Some(Arc::new(StdioTransportInner {
            child,
            stdin: Mutex::new(stdin),
            pending,
            notification_tx,
            next_id: std::sync::atomic::AtomicU64::new(1),
            shutdown: shutdown_tx,
        }));

        self.state.send(ConnectionState::Ready).ok();
        Ok(())
    }

    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let inner = self.inner.as_ref().ok_or(McpError::Disconnected)?;

        // 分配 request id
        let id = inner
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let method = req.method_name.clone();
        let req = JsonRpcRequest::new(id, req.method_name.clone(), req.params.clone());

        // 注册 pending
        let (tx, rx) = oneshot::channel();
        inner.pending.lock().await.insert(id, tx);

        // 通过 stdin 发送
        let json = serde_json::to_string(&req).map_err(|e| McpError::Protocol(e.to_string()))?;
        let mut stdin = inner.stdin.lock().await;
        stdin
            .write_all(json.as_bytes())
            .await
            .map_err(McpError::Io)?;
        stdin.write_all(b"\n").await.map_err(McpError::Io)?;
        stdin.flush().await.map_err(McpError::Io)?;

        // 等待响应（带超时）
        match tokio::time::timeout(self.config.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::Disconnected),
            Err(_elapsed) => {
                // 超时 — 清理 pending entry，避免泄漏
                inner.pending.lock().await.remove(&id);
                tracing::warn!(
                    method = %method,
                    timeout_ms = self.config.request_timeout.as_millis() as u64,
                    "MCP request timed out"
                );
                Err(McpError::Timeout)
            }
        }
    }

    fn notifications(&self) -> NotificationStream {
        if let Some(inner) = &self.inner {
            let rx = inner.notification_tx.subscribe();
            Box::pin(futures_util::stream::unfold(rx, move |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(notif) => break Some((notif, rx)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // 跳过滞后的通知，继续接收
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break None;
                        }
                    }
                }
            }))
        } else {
            Box::pin(futures_util::stream::empty())
        }
    }

    async fn close(&mut self) -> Result<(), McpError> {
        if let Some(ref inner) = self.inner {
            let _ = inner.shutdown.send(true);
        }
        self.inner = None;
        self.state.send(ConnectionState::Closed).ok();
        Ok(())
    }

    fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.state.subscribe()
    }
}
