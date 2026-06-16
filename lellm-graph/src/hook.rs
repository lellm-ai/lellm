//! AgentHook — 可观测性扩展点。
//!
//! 提供 Hook trait，允许用户在 Graph 执行过程中注入可观测性逻辑：
//! - 节点开始/结束回调
//! - 状态变更通知
//! - 自定义指标采集

use std::time::Duration;

use crate::error::ObservedError;
use crate::event::{BarrierDecision, BarrierId};
use crate::node::NextStep;
use crate::state::{SpanId, State, TraceId};

/// Graph 执行 Hook — 可观测性扩展点。
///
/// 所有方法均有默认空实现，实现者只需覆写关心的方法。
///
/// # 设计原则
///
/// - Hook 方法**不影响控制流** — 仅用于观测
/// - 方法内部 panic 不应中断执行 — executor 会 catch
/// - 所有方法都是同步调用（非异步），避免 Hook 阻塞执行
///
/// # 示例
///
/// ```rust,ignore
/// use lellm_graph::{AgentHook, State, SpanId, TraceId};
///
/// struct MetricsHook {
///     start_time: Instant,
/// }
///
/// impl AgentHook for MetricsHook {
///     fn on_node_start(&self, node_name: &str, span_id: SpanId, step: usize) {
///         tracing::info!(node = %node_name, step, "node started");
///     }
///
///     fn on_node_end(&self, node_name: &str, span_id: SpanId, duration: Duration, success: bool) {
///         tracing::info!(node = %node_name, ?duration, success, "node ended");
///     }
/// }
/// ```
pub trait AgentHook: Send + Sync {
    /// 节点开始执行。
    fn on_node_start(&self, _node_name: &str, _span_id: SpanId, _step: usize) {}

    /// 节点执行完成。
    fn on_node_end(&self, _node_name: &str, _span_id: SpanId, _duration: Duration, _success: bool) {
    }

    /// 节点执行失败（错误）。
    fn on_node_failed(&self, _node_name: &str, _error: &str) {}

    /// 状态变更。
    fn on_state_changed(&self, _node_name: &str, _state: &State) {}

    /// 观测错误（不影响控制流）。
    fn on_observed_error(&self, _node_name: &str, _error: &ObservedError) {}

    /// Barrier 等待决策。
    fn on_barrier_waiting(&self, _barrier_id: &BarrierId, _node_name: &str) {}

    /// Barrier 决策已应用。
    fn on_barrier_resolved(&self, _barrier_id: &BarrierId, _decision: &BarrierDecision) {}

    /// 路由决策（节点执行后，决定下一步）。
    fn on_route_decision(&self, _from_node: &str, _next_step: &NextStep, _target: Option<&str>) {}

    /// Graph 执行开始。
    fn on_graph_start(&self, _trace_id: TraceId) {}

    /// Graph 执行完成。
    fn on_graph_complete(&self, _trace_id: TraceId, _duration: Duration) {}

    /// Graph 执行出错。
    fn on_graph_error(&self, _trace_id: TraceId, _error: &str) {}
}

/// 无操作 Hook — 默认行为。
#[derive(Debug, Clone, Default)]
pub struct NoOpHook;

impl AgentHook for NoOpHook {}

/// 日志 Hook — 将所有事件输出为 tracing 日志。
#[derive(Debug, Clone)]
pub struct TracingHook;

impl AgentHook for TracingHook {
    fn on_node_start(&self, node_name: &str, span_id: SpanId, step: usize) {
        tracing::debug!(node = %node_name, span = %span_id.0, step, "node start");
    }

    fn on_node_end(&self, node_name: &str, span_id: SpanId, duration: Duration, success: bool) {
        if success {
            tracing::debug!(
                node = %node_name,
                span = %span_id.0,
                duration_ms = duration.as_millis(),
                "node end"
            );
        } else {
            tracing::warn!(
                node = %node_name,
                span = %span_id.0,
                duration_ms = duration.as_millis(),
                "node failed"
            );
        }
    }

    fn on_node_failed(&self, node_name: &str, error: &str) {
        tracing::error!(node = %node_name, error = %error, "node execution failed");
    }

    fn on_observed_error(&self, node_name: &str, error: &ObservedError) {
        tracing::warn!(node = %node_name, error = %error, "observed error");
    }

    fn on_barrier_waiting(&self, barrier_id: &BarrierId, node_name: &str) {
        tracing::info!(
            barrier = %barrier_id.node_id,
            occurrence = barrier_id.occurrence,
            node = %node_name,
            "barrier waiting for decision"
        );
    }

    fn on_barrier_resolved(&self, barrier_id: &BarrierId, decision: &BarrierDecision) {
        tracing::info!(
            barrier = %barrier_id.node_id,
            occurrence = barrier_id.occurrence,
            decision = ?decision,
            "barrier resolved"
        );
    }

    fn on_route_decision(&self, from_node: &str, next_step: &NextStep, target: Option<&str>) {
        tracing::debug!(
            from = %from_node,
            next_step = ?next_step,
            target = target.unwrap_or("N/A"),
            "route decision"
        );
    }

    fn on_graph_start(&self, trace_id: TraceId) {
        tracing::info!(trace = %trace_id.0, "graph execution start");
    }

    fn on_graph_complete(&self, trace_id: TraceId, duration: Duration) {
        tracing::info!(
            trace = %trace_id.0,
            duration_ms = duration.as_millis(),
            "graph execution complete"
        );
    }

    fn on_graph_error(&self, trace_id: TraceId, error: &str) {
        tracing::error!(trace = %trace_id.0, error = %error, "graph execution error");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_hook() {
        let hook = NoOpHook;
        hook.on_node_start("test", SpanId::new(), 1);
        hook.on_node_end("test", SpanId::new(), Duration::from_secs(1), true);
        hook.on_graph_start(TraceId::default());
        // NoOpHook 不 panic
    }

    #[test]
    fn test_tracing_hook() {
        let hook = TracingHook;
        hook.on_node_start("test", SpanId::new(), 1);
        hook.on_node_end("test", SpanId::new(), Duration::from_secs(1), true);
        hook.on_graph_start(TraceId::default());
        hook.on_graph_complete(TraceId::default(), Duration::from_secs(1));
        // TracingHook 不 panic
    }

    #[test]
    fn test_custom_hook() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingHook {
            starts: Arc<AtomicUsize>,
            ends: Arc<AtomicUsize>,
        }

        impl AgentHook for CountingHook {
            fn on_node_start(&self, _node_name: &str, _span_id: SpanId, _step: usize) {
                self.starts.fetch_add(1, Ordering::Relaxed);
            }

            fn on_node_end(
                &self,
                _node_name: &str,
                _span_id: SpanId,
                _duration: Duration,
                _success: bool,
            ) {
                self.ends.fetch_add(1, Ordering::Relaxed);
            }
        }

        let starts = Arc::new(AtomicUsize::new(0));
        let ends = Arc::new(AtomicUsize::new(0));
        let hook = CountingHook {
            starts: starts.clone(),
            ends: ends.clone(),
        };

        hook.on_node_start("a", SpanId::new(), 1);
        hook.on_node_start("b", SpanId::new(), 2);
        hook.on_node_end("a", SpanId::new(), Duration::ZERO, true);

        assert_eq!(starts.load(Ordering::Relaxed), 2);
        assert_eq!(ends.load(Ordering::Relaxed), 1);
    }
}
