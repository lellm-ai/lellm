//! 工具系统 — 注册、定义、执行。
//!
//! 独立的工具子系统，被 runtime 层使用。

mod args;
mod executor;

pub use args::ToolArgs;
pub use executor::{
    BatchExecutionResult, ParallelSafety, ToolCategory, ToolExecutor, ToolRegistration,
};

/// 异步工具函数类型（executor 内部使用）
pub(crate) type ToolFn = std::sync::Arc<
    dyn Fn(
            &serde_json::Value,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = super::ToolResult> + Send>>
        + Send
        + Sync,
>;
