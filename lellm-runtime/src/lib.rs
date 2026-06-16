//! lellm-runtime — 运行时基础设施。
//!
//! 全系统共享的基础设施层，无外部 LLM 依赖。
//! 仅提供 State 管理、Delta 合并、Checkpoint 恢复、执行追踪等核心能力。
//!
//! **依赖关系：** 仅依赖 serde / serde_json / thiserror / tracing / uuid

pub mod checkpoint;
pub mod delta;
pub mod state;
pub mod statekey;

// ─── State ──────────────────────────────────────────────────────
pub use state::{State, StateError, StateExt, StateReducer, SpanId, array_reducer};

// ─── Delta + Reducer ────────────────────────────────────────────
pub use delta::{DeltaOp, Reducer, ReducerRegistry, StateDelta};

// ─── StateKey ───────────────────────────────────────────────────
pub use statekey::{StateKey, StateKeyExt};

// ─── Checkpoint + Trace ─────────────────────────────────────────
pub use checkpoint::{
    BarrierDecision, Checkpoint, CheckpointId, ExecutionEntry, ExecutionTrace,
    GraphHashMode, GraphResult, NodeId, TraceId,
};
