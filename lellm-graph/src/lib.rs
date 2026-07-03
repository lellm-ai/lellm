//! lellm-graph — Graph/Node/Edge 编排层 + 状态管理 + Checkpoint。
//!
//! 通用工作流引擎（类似 LangGraph / Temporal / Prefect）。

// ─── Domain Modules ──────────────────────────────────────────
pub mod checkpoint;
pub mod compiler;
pub mod exec;
pub mod graph;
pub mod node;
pub mod state;

// ─── Root Modules ────────────────────────────────────────────
pub mod error;
pub mod event;
pub mod ids;
pub mod runtime_event;
pub mod stream_chunk;
pub mod stream_emitter;
pub mod test_executor;

// ─── IDs ─────────────────────────────────────────────────────
pub use checkpoint::TraceId;
pub use ids::SpanId;

// ─── State ───────────────────────────────────────────────────
pub use state::{
    ExecutionEntry, GraphResult, State, StateError, StateExt, StateMerge, StateMutation,
    StateReducer, array_reducer,
};

// ─── StateKey ────────────────────────────────────────────────
pub use state::{
    Reducer, SK_COUNT, SK_ITERATIONS, SK_MESSAGES, SK_OUTPUT_TOKENS, SK_PENDING_TOOL_CALLS,
    SK_REASONING_TOKENS, SK_STEPS, SK_TOTAL_TOOL_CALLS, StateKey, StateKeyExt,
};

// ─── Checkpoint ──────────────────────────────────────────────
#[allow(deprecated)]
pub use checkpoint::{
    Checkpoint, CheckpointBlob, CheckpointId, CheckpointPolicy, CheckpointSink,
    CheckpointStoreError, FrameInfo, MemorySink, NodeId, NoopCheckpointSink,
};

// ─── Checkpoint Policy ───────────────────────────────────────
pub use checkpoint::{RetentionPolicy, TriggerPolicy};

// ─── Barrier Sink ────────────────────────────────────────────
pub use node::{BarrierOutcome, BarrierSink, ChannelBarrierSink, MockBarrierSink, NoopBarrierSink};

// ─── Checkpoint Codec ────────────────────────────────────────
pub use checkpoint::{CheckpointCodec, SerdeCheckpointCodec, TypedCheckpointStore};

// ─── Store ───────────────────────────────────────────────────
pub use checkpoint::{BlobCheckpointStore, InMemoryBlobStore};

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
pub use graph::CycleAnalysis;
pub use graph::GraphBuilder;
pub use graph::{Edge, Graph, NoopStepCallback, StepCallback};

// ─── Nodes ───────────────────────────────────────────────────
pub use node::{
    BarrierDefaultAction, BarrierNode, BranchCondition, ConditionNode, ConditionNodeBuilder,
    ExecutorOperation, FlowNode, LeafNode, NodeKind, ParallelErrorStrategy, ParallelNode,
    ParallelNodeBuilder, TaskFn, TaskNode,
};

// ─── CompiledSubgraph + StateProjector ─────────────────────
pub use node::{CompiledSubgraph, StateProjector};

// ─── StateLens + SubgraphSpec ──────────────────────────────
pub use node::SubgraphSpec;
pub use state::{IdentityLens, StateLens};

// ─── ExecutionSession + SessionCheckpoint + SessionError ────
pub use checkpoint::{Frame, FrameStack};
pub use exec::{ExecutionSession, SessionCheckpoint, SessionCheckpointSink, SessionError};

// ─── Test Executor (SimpleExecutor 兼容层) ────────────────────
pub use test_executor::SimpleExecutor;

// ─── v04: ExecutionEngine + NodeContext + Stream ─────────────
pub use exec::{
    ExecutionContext, ExecutionControl, ExecutionEngine, ExecutionSignal, ExecutionView,
    ExecutorState, NextAction, NodeMetadata, OwnedExecutionEngine,
};
pub use node::{LeafContext, NodeContext};
pub use runtime_event::RuntimeEvent;
pub use state::{LastWriteWins, MergeStrategy, WorkflowError, WorkflowState};
pub use stream_chunk::{StreamChunk, ToolPhase};
pub use stream_emitter::{
    BufferedSink, ChannelSink, NoopSink, StreamHub, StreamSink, noop_sink, sink_arc,
    spawn_forward_task,
};
pub use tokio_util::sync::CancellationToken;

// ─── Trace ───────────────────────────────────────────────────
pub use checkpoint::{
    ExecutionTrace, ExportedTrace, ExportedTraceStep, MemoryTraceSink, TraceSink, TraceStep,
};

// ─── MutationLog ─────────────────────────────────────────────
pub use checkpoint::{
    InMemoryMutationLog, MutationLogEntry, MutationLogStore, mutations_to_log_entries,
};
