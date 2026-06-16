//! State 和执行结果。
//!
//! 核心类型从 `lellm-runtime` re-export，本模块仅保留 Graph 特有的执行结果类型。

use std::time::{Duration, Instant};

// ─── Re-export from lellm-runtime ──────────────────────────────

pub use lellm_runtime::{
    ReducerRegistry, SpanId, State, StateDelta, StateError, StateExt, StateReducer, TraceId,
    array_reducer,
};

/// Graph 执行结果。
#[derive(Debug)]
pub struct GraphResult {
    /// 执行追踪 ID（关联本次执行的所有 SpanId）
    pub trace_id: TraceId,
    /// 最终状态
    pub state: State,
    /// 执行日志
    pub execution_log: Vec<ExecutionEntry>,
    /// 执行耗时
    pub duration: Duration,
}

/// 单个节点执行记录。
#[derive(Debug, Clone)]
pub struct ExecutionEntry {
    /// 全局步数（第几步）
    pub step: usize,
    /// 节点名称
    pub node_name: String,
    /// 开始时间
    pub start_time: Instant,
    /// 结束时间
    pub end_time: Instant,
    /// 是否成功
    pub success: bool,
}

impl ExecutionEntry {
    /// 执行耗时
    pub fn elapsed(&self) -> Duration {
        self.end_time.duration_since(self.start_time)
    }
}
