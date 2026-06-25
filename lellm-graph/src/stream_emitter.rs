//! StreamSink — 数据面发射抽象。
//!
//! Graph 层只知道 `StreamSink` trait，不知道 channel、WebSocket、Logger。
//! 所有消费端实现都在 Agent/Provider 层。
//!
//! 设计原则：
//! - 同步 `emit` — Node 永远不阻塞（O(1)）
//! - Producer Push 模型 — 生产者推送，不感知消费者
//! - 取消 = 消费者离开（不是背压）

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::stream_chunk::StreamChunk;

// ─── StreamSink Trait ─────────────────────────────────────────

/// 数据面发射抽象。
///
/// Graph 层唯一的流式依赖。Node 通过 `ctx.emit(chunk)` 推送数据。
/// 实现者决定如何处理（channel、WebSocket、文件、丢弃）。
pub trait StreamSink: Send + Sync {
    /// 发射一个数据面事件。
    ///
    /// 同步调用，永远不阻塞。
    fn emit(&self, chunk: StreamChunk);
}

// ─── BufferedSink ─────────────────────────────────────────────

/// 基于大缓冲队列的 StreamSink 实现。
///
/// 用于 Agent 层：将 StreamChunk 推入队列，
/// 由 Forward Task 异步消费并转发到 mpsc channel。
///
/// ```text
/// LLMNode
///    ↓ emit() — O(1), 固定成本
/// BufferedSink (large buffer mpsc)
///    ↓
/// Forward Task (spawn)
///    ↓
/// mpsc::Sender<StreamChunk> (bounded, backpressure)
///    ↓
/// Consumer
/// ```
pub struct BufferedSink {
    tx: mpsc::UnboundedSender<StreamChunk>,
}

impl BufferedSink {
    /// 创建 BufferedSink（无界队列）。
    ///
    /// 取消机制负责清理：消费者离开 → cancel → Node 停止。
    pub fn new() -> (Self, mpsc::UnboundedReceiver<StreamChunk>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl StreamSink for BufferedSink {
    fn emit(&self, chunk: StreamChunk) {
        // unbounded send only fails if receiver is dropped
        let _ = self.tx.send(chunk);
    }
}

impl Clone for BufferedSink {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
        }
    }
}

// ─── ChannelSink ──────────────────────────────────────────────

/// 直接写入 mpsc channel 的 StreamSink。
///
/// 用于测试或简单场景——不需要 Forward Task。
/// channel full 时静默丢弃（消费者会触发 cancel）。
pub struct ChannelSink {
    tx: mpsc::Sender<StreamChunk>,
}

impl ChannelSink {
    pub fn new(tx: mpsc::Sender<StreamChunk>) -> Self {
        Self { tx }
    }
}

impl StreamSink for ChannelSink {
    fn emit(&self, chunk: StreamChunk) {
        // try_send 失败 = channel full 或消费者已断开
        // 消费者断开时，cancel 会触发，Node 会停止
        let _ = self.tx.try_send(chunk);
    }
}

// ─── NoopSink ─────────────────────────────────────────────────

/// 丢弃所有事件的 StreamSink。
///
/// 用于阻塞执行模式（sink=None 的等价实现）和测试。
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSink;

impl StreamSink for NoopSink {
    fn emit(&self, _chunk: StreamChunk) {
        // no-op
    }
}

// ─── Arc<dyn StreamSink> helpers ──────────────────────────────

/// 创建 `Arc<dyn StreamSink>` 的便捷函数。
pub fn sink_arc<S: StreamSink + 'static>(sink: S) -> Arc<dyn StreamSink> {
    Arc::new(sink)
}

/// 创建 NoopSink 的 Arc。
pub fn noop_sink() -> Arc<dyn StreamSink> {
    Arc::new(NoopSink)
}

// ─── Forward Task ─────────────────────────────────────────────

/// 启动 Forward Task：从 BufferedSink 读取，转发到 mpsc channel。
///
/// 消费者断开（Receiver dropped）时，task 退出并触发 CancellationToken。
pub fn spawn_forward_task(
    mut buffer_rx: mpsc::UnboundedReceiver<StreamChunk>,
    tx: mpsc::Sender<StreamChunk>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                chunk = buffer_rx.recv() => {
                    let chunk = match chunk {
                        Some(c) => c,
                        None => break, // sender dropped
                    };

                    // 发送失败 = 消费者断开
                    if tx.send(chunk).await.is_err() {
                        break;
                    }
                }
            }
        }
        // Forward task 退出 → 触发取消
        cancel.cancel();
    })
}
