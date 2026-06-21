//! lellm-graph — Graph/Node/Edge 编排层 + 状态管理 + Checkpoint。
//!
//! 通用工作流引擎（类似 LangGraph / Temporal / Prefect）。
//! 吸收了原 lellm-runtime，包含 State/StateDelta/Checkpoint 等基础设施。
//! 依赖 `lellm-core`，不依赖 agent/provider。

pub mod barrier_node;
pub mod branch_state;
pub mod checkpoint;
pub mod delta;
pub mod error;
pub mod event;
pub mod executor;
pub mod graph;
pub mod hook;
pub mod ids;
pub mod node;
pub mod node_context;
pub mod parallel_node;
pub mod runtime_event;
pub mod state;
pub mod statekey;
pub mod store;
pub mod stream_chunk;
pub mod stream_emitter;
pub mod workflow_state;

// ─── IDs ─────────────────────────────────────────────────────
pub use ids::{SpanId, TraceId};

// ─── State ───────────────────────────────────────────────────
pub use state::{
    ExecutionEntry, GraphResult, State, StateEffect, StateError, StateExt, StateReducer,
    array_reducer,
};

// ─── Delta + Reducer ─────────────────────────────────────────
pub use delta::{DeltaOp, DeltaSource, Reducer, ReducerRegistry, StateDelta};

// ─── StateKey ────────────────────────────────────────────────
pub use statekey::{
    SK_COUNT, SK_ITERATIONS, SK_MESSAGES, SK_OUTPUT_TOKENS, SK_PENDING_TOOL_CALLS,
    SK_REASONING_TOKENS, SK_STEPS, SK_TOTAL_TOOL_CALLS, StateKey, StateKeyExt,
};

// ─── Checkpoint ──────────────────────────────────────────────
pub use checkpoint::{
    BarrierDecisionRecord, Checkpoint, CheckpointId, CheckpointPolicy, CheckpointScore,
    CheckpointStore, CheckpointStoreError, CheckpointTrigger, ExecutionMetadata, ExecutionTrace,
    GraphHashMode, IncrementalSnapshotState, NodeId, StateSnapshot,
};

// ─── Store ───────────────────────────────────────────────────
pub use store::InMemoryCheckpointStore;

// ─── Error Types ─────────────────────────────────────────────
pub use error::{
    BuildError, BuildErrors, Diagnostic, DiagnosticCategory, DiagnosticSeverity, GraphDiagnostics,
    GraphError, ObservedError, TerminalError,
};

// ─── Events ──────────────────────────────────────────────────
pub use event::{
    BarrierDecision, BarrierId, FlowEvent, GraphEvent, GraphExecution, GraphHandle, GraphStream,
};

// ─── Graph ───────────────────────────────────────────────────
pub use graph::{CycleAnalysis, Edge, Graph, GraphBuilder};

// ─── Nodes ───────────────────────────────────────────────────
pub use node::{
    BarrierDefaultAction, BarrierNode, BranchCondition, ConditionNode, ConditionNodeBuilder,
    FlowNode, NextStep, NodeKind, NodeOutput, ParallelErrorStrategy, ParallelNode,
    ParallelNodeBuilder, TaskFn, TaskNode,
};

// ─── Executor ────────────────────────────────────────────────
pub use executor::GraphExecutor;

// ─── Hooks ───────────────────────────────────────────────────
pub use hook::{AgentHook, NoOpHook, TracingHook};

// ─── v04: NodeContext + BranchState + Stream ──────────────────
pub use branch_state::{BranchState, ChangeOperation, ChangeRecord};
pub use node_context::{ExecutionControl, ExecutionSignal, NextAction, NodeContext, NodeMetadata};
pub use runtime_event::RuntimeEvent;
pub use stream_chunk::StreamChunk;
pub use stream_emitter::StreamEmitter;
pub use workflow_state::{Effect, WorkflowError, WorkflowState};
