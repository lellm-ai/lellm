//! lellm-graph — Graph/Node/Edge 编排层。
//!
//! 通用工作流引擎（类似 LangGraph / Temporal / Prefect）。
//! 依赖 `lellm-runtime`，不依赖 agent/provider/core。

pub mod barrier_node;
pub mod error;
pub mod event;
pub mod executor;
pub mod graph;
pub mod hook;
pub mod node;
pub mod parallel_node;
pub mod state;
pub mod statekey;

// ─── Re-export from lellm-runtime ─────────────────────────────
pub use lellm_runtime::{
    Checkpoint, CheckpointId, CheckpointPolicy, CheckpointScore, CheckpointStore,
    CheckpointStoreError, CheckpointTrigger, DeltaOp, ExecutionMetadata, ExecutionTrace,
    GraphHashMode, InMemoryCheckpointStore, Reducer, ReducerRegistry, SpanId, State, StateDelta,
    StateError, StateExt, StateKey, StateKeyExt, StateReducer, TraceId, array_reducer,
};

// ─── Error Types ────────────────────────────────────────────────
pub use error::{
    BuildError, BuildErrors, Diagnostic, DiagnosticCategory, DiagnosticSeverity, GraphDiagnostics,
    GraphError, ObservedError, TerminalError,
};

// ─── Events ─────────────────────────────────────────────────────
pub use event::{
    BarrierDecision, BarrierId, FlowEvent, GraphEvent, GraphExecution, GraphHandle, GraphStream,
};

// ─── Graph ──────────────────────────────────────────────────────
pub use graph::{CycleAnalysis, Edge, Graph, GraphBuilder};

// ─── Nodes ──────────────────────────────────────────────────────
pub use node::{
    BarrierDefaultAction, BarrierNode, BranchCondition, ConditionNode, ConditionNodeBuilder,
    FlowNode, NextStep, NodeKind, NodeOutput, ParallelErrorStrategy, ParallelNode, TaskFn,
    TaskNode,
};

// ─── State (graph-specific) ─────────────────────────────────────
pub use state::{ExecutionEntry, GraphResult};

// ─── StateKey (built-in constants) ──────────────────────────────
pub use statekey::{SK_COUNT, SK_MESSAGES, SK_STEPS};

// ─── Executor ───────────────────────────────────────────────────
pub use executor::GraphExecutor;

// ─── Hooks ──────────────────────────────────────────────────────
pub use hook::{AgentHook, NoOpHook, TracingHook};
