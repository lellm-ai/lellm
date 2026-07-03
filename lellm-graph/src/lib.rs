//! lellm-graph — Graph/Node/Edge 编排层 + 状态管理 + Checkpoint。
//!
//! 通用工作流引擎（类似 LangGraph / Temporal / Prefect）。

pub mod barrier_node;
pub mod barrier_wait;
pub mod checkpoint;
pub mod checkpoint_codec;
pub mod checkpoint_policy;
pub mod compiled_subgraph;
pub mod compiler;
pub mod error;
pub mod event;
pub mod execution_engine;
pub mod execution_loop;
pub mod graph;
pub mod graph_analysis;
pub mod graph_builder;
pub mod ids;
pub mod mutation_log;
pub mod node;
pub mod node_context;
pub mod owned_execution_engine;
pub mod parallel_node;
pub mod runtime_event;
pub mod session;
pub mod state;
pub mod state_lens;
pub mod statekey;
pub mod store;
pub mod stream_chunk;
pub mod stream_emitter;
pub mod subgraph_spec;
pub mod test_executor;
pub mod workflow_state;

// ─── IDs ─────────────────────────────────────────────────────
pub use checkpoint::TraceId;
pub use ids::SpanId;

// ─── State ───────────────────────────────────────────────────
pub use state::{
    ExecutionEntry, GraphResult, State, StateError, StateExt, StateMerge, StateMutation,
    StateReducer, array_reducer,
};

// ─── StateKey ────────────────────────────────────────────────
pub use statekey::{
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
pub use checkpoint_policy::{RetentionPolicy, TriggerPolicy};

// ─── Checkpoint Codec ────────────────────────────────────────
pub use checkpoint_codec::{CheckpointCodec, SerdeCheckpointCodec, TypedCheckpointStore};

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
pub use graph::{Edge, Graph};
pub use graph_analysis::CycleAnalysis;
pub use graph_builder::GraphBuilder;

// ─── Nodes ───────────────────────────────────────────────────
pub use node::{
    BarrierDefaultAction, BarrierNode, BranchCondition, ConditionNode, ConditionNodeBuilder,
    ExecutorOperation, FlowNode, LeafNode, NodeKind, ParallelErrorStrategy, ParallelNode,
    ParallelNodeBuilder, TaskFn, TaskNode,
};

// ─── CompiledSubgraph + StateProjector ─────────────────────
pub use compiled_subgraph::{CompiledSubgraph, StateProjector};

// ─── StateLens + SubgraphSpec ──────────────────────────────
pub use state_lens::{IdentityLens, StateLens};
pub use subgraph_spec::SubgraphSpec;

// ─── ExecutionSession + SessionCheckpoint + SessionError ────
pub use checkpoint::{Frame, FrameStack};
pub use session::{ExecutionSession, SessionCheckpoint, SessionCheckpointSink, SessionError};

// ─── Test Executor (SimpleExecutor 兼容层) ────────────────────
pub use test_executor::SimpleExecutor;

// ─── v04: ExecutionEngine + NodeContext + Stream ─────────────
pub use execution_engine::{
    ExecutionContext, ExecutionControl, ExecutionEngine, ExecutionSignal, ExecutionView,
    ExecutorState, NextAction, NodeMetadata, OwnedExecutionEngine,
};
pub use node_context::{LeafContext, NodeContext};
pub use runtime_event::RuntimeEvent;
pub use stream_chunk::{StreamChunk, ToolPhase};
pub use stream_emitter::{
    BufferedSink, ChannelSink, NoopSink, StreamHub, StreamSink, noop_sink, sink_arc,
    spawn_forward_task,
};
pub use tokio_util::sync::CancellationToken;
pub use workflow_state::{LastWriteWins, MergeStrategy, WorkflowError, WorkflowState};

// ─── Trace ───────────────────────────────────────────────────
pub mod trace;
pub use trace::{
    ExecutionTrace, ExportedTrace, ExportedTraceStep, MemoryTraceSink, TraceSink, TraceStep,
};

// ─── MutationLog ─────────────────────────────────────────────
pub use mutation_log::{
    InMemoryMutationLog, MutationLogEntry, MutationLogStore, mutations_to_log_entries,
};
