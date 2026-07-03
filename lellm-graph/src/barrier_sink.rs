//! BarrierSink — Barrier 等待的 Sink 抽象。
//!
//! 设计理念：所有高级能力（Trace、Checkpoint、Barrier）统一通过 Sink 注入 Engine，
//! 避免 Runtime 维护任何"事件缓冲"或第二套执行循环。
//!
//! ```text
//! Graph::run_inline()
//!         │
//!         ▼
//!    ExecutionEngine
//!         │
//!         ├── StreamSink        — 数据面流式输出
//!         ├── CheckpointSink    — 恢复边界通知
//!         ├── BarrierSink       — Barrier 等待 + 决策注入
//!         └── MetricsSink       — (未来)
//! ```

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::event::{BarrierDecision, BarrierDecisionMessage, BarrierId};

// ─── BarrierOutcome ────────────────────────────────────────────

/// Barrier 等待的结果。
#[derive(Debug, Clone)]
pub enum BarrierOutcome {
    /// 收到决策
    Decision(BarrierDecision),
    /// 超时
    TimedOut,
    /// 取消
    Cancelled,
}

// ─── BarrierSink ───────────────────────────────────────────────

/// Barrier 等待接收器 — Engine 通过 BarrierSink 等待外部决策。
///
/// 与 CheckpointSink 类似，Engine 借用 Sink，不拥有生命周期。
/// Sink 决定如何等待决策（通道、回调、模拟等）。
pub trait BarrierSink: Send + Sync {
    /// 等待 Barrier 决策。
    ///
    /// 返回 `Pin<Box<dyn Future<...>>>`，使用 `'_`  lifetime 以支持借用 `&self`。
    fn wait_decision(
        &self,
        barrier_id: &BarrierId,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = BarrierOutcome> + Send + '_>>;
}

// ─── NoopBarrierSink ───────────────────────────────────────────

/// 空 Sink — 直接 Approve，不等待。
pub struct NoopBarrierSink;

impl BarrierSink for NoopBarrierSink {
    fn wait_decision(
        &self,
        _barrier_id: &BarrierId,
        _timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = BarrierOutcome> + Send + '_>> {
        Box::pin(async { BarrierOutcome::Decision(BarrierDecision::Approve) })
    }
}

// ─── MockBarrierSink ───────────────────────────────────────────

/// 模拟 Sink — 返回预设决策，用于测试。
pub struct MockBarrierSink {
    pub decision: BarrierDecision,
}

impl MockBarrierSink {
    pub fn new(decision: BarrierDecision) -> Self {
        Self { decision }
    }
}

impl BarrierSink for MockBarrierSink {
    fn wait_decision(
        &self,
        _barrier_id: &BarrierId,
        _timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = BarrierOutcome> + Send + '_>> {
        let decision = self.decision.clone();
        Box::pin(async { BarrierOutcome::Decision(decision) })
    }
}

// ─── SharedReceiver ────────────────────────────────────────────

/// 共享的 mpsc Receiver — 通过 Arc + tokio Mutex 实现多消费者。
struct SharedReceiver<T: Send> {
    inner: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<T>>>,
}

impl<T: Send> SharedReceiver<T> {
    fn new(rx: tokio::sync::mpsc::Receiver<T>) -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }

    async fn recv(&self) -> Option<T> {
        let mut guard = self.inner.lock().await;
        guard.recv().await
    }
}

impl<T: Send> Clone for SharedReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

// ─── ChannelBarrierSink ────────────────────────────────────────

/// 通道 Sink — 通过 mpsc 通道等待决策。
///
/// 生产环境使用，与 `GraphHandle::decide()` 配合。
pub struct ChannelBarrierSink {
    decision_rx: SharedReceiver<BarrierDecisionMessage>,
    cancel_rx: SharedReceiver<()>,
    cancel: Arc<tokio_util::sync::CancellationToken>,
    wildcard_cache: Arc<tokio::sync::RwLock<std::collections::HashMap<String, BarrierDecision>>>,
}

impl ChannelBarrierSink {
    pub(crate) fn new(
        decision_rx: tokio::sync::mpsc::Receiver<BarrierDecisionMessage>,
        cancel_rx: tokio::sync::mpsc::Receiver<()>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            decision_rx: SharedReceiver::new(decision_rx),
            cancel_rx: SharedReceiver::new(cancel_rx),
            cancel: Arc::new(cancel),
            wildcard_cache: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }
}

impl BarrierSink for ChannelBarrierSink {
    fn wait_decision(
        &self,
        barrier_id: &BarrierId,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = BarrierOutcome> + Send + '_>> {
        // 先检查通配缓存 — clone 决策后再返回，避免借用冲突
        {
            let cache_guard = self.wildcard_cache.try_read();
            if let Ok(cache) = cache_guard {
                if let Some(decision) = cache.get(&barrier_id.node_id) {
                    let decision = decision.clone();
                    return Box::pin(async { BarrierOutcome::Decision(decision) });
                }
            }
        }

        let decision_rx = self.decision_rx.clone();
        let cancel_rx = self.cancel_rx.clone();
        let cancel = self.cancel.clone();
        let wildcard_cache = self.wildcard_cache.clone();
        let barrier_id = barrier_id.clone();

        Box::pin(async move {
            let outcome = if let Some(dur) = timeout {
                tokio::select! {
                    biased;
                    _ = cancel_rx.recv() => {
                        cancel.cancel();
                        BarrierOutcome::Cancelled
                    }
                    _ = tokio::time::sleep(dur) => BarrierOutcome::TimedOut,
                    msg = decision_rx.recv() => match msg {
                        Some(BarrierDecisionMessage::Exact { barrier_id: bid, decision }) => {
                            if bid == barrier_id {
                                BarrierOutcome::Decision(decision)
                            } else {
                                BarrierOutcome::Cancelled
                            }
                        }
                        Some(BarrierDecisionMessage::Wildcard { node_id, decision }) => {
                            if node_id == barrier_id.node_id {
                                wildcard_cache.write().await.insert(node_id.clone(), decision.clone());
                                BarrierOutcome::Decision(decision)
                            } else {
                                BarrierOutcome::Cancelled
                            }
                        }
                        None => BarrierOutcome::Cancelled,
                    },
                }
            } else {
                tokio::select! {
                    biased;
                    _ = cancel_rx.recv() => {
                        cancel.cancel();
                        BarrierOutcome::Cancelled
                    }
                    msg = decision_rx.recv() => match msg {
                        Some(BarrierDecisionMessage::Exact { barrier_id: bid, decision }) => {
                            if bid == barrier_id {
                                BarrierOutcome::Decision(decision)
                            } else {
                                BarrierOutcome::Cancelled
                            }
                        }
                        Some(BarrierDecisionMessage::Wildcard { node_id, decision }) => {
                            if node_id == barrier_id.node_id {
                                wildcard_cache.write().await.insert(node_id.clone(), decision.clone());
                                BarrierOutcome::Decision(decision)
                            } else {
                                BarrierOutcome::Cancelled
                            }
                        }
                        None => BarrierOutcome::Cancelled,
                    },
                }
            };
            outcome
        })
    }
}
