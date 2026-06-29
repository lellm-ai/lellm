//! lellm-graph — Graph/Node/Edge 编排层 + 状态管理 + Checkpoint。
//!
//! 通用工作流引擎（类似 LangGraph / Temporal / Prefect）。

pub mod barrier_node;
pub mod checkpoint;
pub mod checkpoint_codec;
pub mod error;
pub mod event;
pub mod execution_loop;
pub mod graph;
pub mod graph_analysis;
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
pub mod test_executor;
pub mod workflow_state;

// ─── IDs ─────────────────────────────────────────────────────
pub use checkpoint::TraceId;
pub use ids::SpanId;

// ─── State ───────────────────────────────────────────────────
pub use state::{
    ExecutionEntry, GraphResult, State, StateMutation, StateError, StateExt, StateMerge,
    StateReducer, array_reducer,
};

// ─── StateKey ────────────────────────────────────────────────
pub use statekey::{
    Reducer, SK_COUNT, SK_ITERATIONS, SK_MESSAGES, SK_OUTPUT_TOKENS, SK_PENDING_TOOL_CALLS,
    SK_REASONING_TOKENS, SK_STEPS, SK_TOTAL_TOOL_CALLS, StateKey, StateKeyExt,
};

// ─── Checkpoint ──────────────────────────────────────────────
pub use checkpoint::{
    Checkpoint, CheckpointBlob, CheckpointId, CheckpointPolicy, CheckpointStoreError, NodeId,
};

// ─── Checkpoint Codec ────────────────────────────────────────
pub use checkpoint_codec::{
    CheckpointCodec, SerdeCheckpointCodec, TypedCheckpointStore,
};

// ─── Store ───────────────────────────────────────────────────
pub use store::{BlobCheckpointStore, InMemoryBlobStore};

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
pub use graph::{Edge, Graph, GraphBuilder};
pub use graph_analysis::CycleAnalysis;

// ─── Nodes ───────────────────────────────────────────────────
pub use node::{
    BarrierDefaultAction, BarrierNode, BranchCondition, ConditionNode, ConditionNodeBuilder,
    FlowNode, NodeKind, ParallelErrorStrategy, ParallelNode,
    ParallelNodeBuilder, TaskFn, TaskNode, ExecutorOperation, LeafNode,
};

// ─── Test Executor (SimpleExecutor 兼容层) ────────────────────
pub use test_executor::SimpleExecutor;

// ─── v04: NodeContext + Stream ───────────────────────────────
pub use node_context::{
    ExecutionEngine, ExecutionContext, ExecutionControl, ExecutionSignal, ExecutorState,
    ExecutionView, NextAction, NodeContext, NodeMetadata, LeafContext,
};
pub use runtime_event::RuntimeEvent;
pub use stream_chunk::{StreamChunk, ToolPhase};
pub use stream_emitter::{
    BufferedSink, ChannelSink, NoopSink, StreamHub, StreamSink, noop_sink, sink_arc, spawn_forward_task,
};
pub use tokio_util::sync::CancellationToken;
pub use workflow_state::{LastWriteWins, MergeStrategy, WorkflowError, WorkflowState};

// ─── Trace ───────────────────────────────────────────────────
pub mod trace;
pub use trace::{
    ExecutionTrace, ExportedTrace, ExportedTraceStep, MemoryTraceSink, TraceSink, TraceStep,
};
