//! 上下文预算管理 — 控制 Agent Loop 中 messages 的 Token 总量。
//!
//! 核心设计：
//! - `ContextBudget` — 纯参数配置，用户可调
//! - `ContextCompactor` — trait，可插拔压缩策略
//! - `LocalCompactor` — 默认实现，滑动窗口 + 本地摘要
//! - `AgentExecutionContext` — 执行期运行时缓存（替代 LoopState.estimated_tokens）
//! - **Assistant(tool_call) + ToolResult 是原子块，不可拆分**

mod budget;
mod compactor;
mod estimation;
mod execution;
mod local_compactor;

pub use budget::ContextBudget;
pub use compactor::{CompactionResult, ContextCompactor};
pub use estimation::{estimate_message, estimate_reasoning_block, estimate_text, estimate_tokens};
pub use execution::AgentExecutionContext;
pub use local_compactor::LocalCompactor;
