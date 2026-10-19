//! 工具执行身份；不依赖 Graph 或异步运行时。
use crate::ToolResult;
use std::{future::Future, pin::Pin, sync::Arc};

/// 重放安全性与 ParallelSafety 是两个独立维度。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolReplayPolicy {
    /// 未声明的工具出现未知结果时必须先对账。
    #[default]
    ManualRecovery,
    /// 只读或业务保证重复调用安全。
    ReplaySafe,
    /// 远端按传入的 idempotency_key 去重；必须使用上下文执行器。
    IdempotentWithKey,
}

#[derive(Clone)]
pub struct ToolExecutionContext {
    pub task_id: String,
    pub idempotency_key: String,
    pub attempt: u32,
    cancellation: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl ToolExecutionContext {
    pub fn new(task_id: String, idempotency_key: String, attempt: u32) -> Self {
        Self {
            task_id,
            idempotency_key,
            attempt,
            cancellation: Arc::new(|| false),
        }
    }
    pub fn with_cancellation(mut self, check: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        self.cancellation = Arc::new(check);
        self
    }
    pub fn is_cancelled(&self) -> bool {
        (self.cancellation)()
    }
}

pub type ContextToolFn = Arc<
    dyn Fn(
            &serde_json::Value,
            &ToolExecutionContext,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send>>
        + Send
        + Sync,
>;
