//! lellm-graph — Graph/Node/Edge 编排层。
//!
//! 提供 Workflow Graph + Human-in-the-loop 编排能力。

pub mod barrier_node;
pub mod error;
pub mod event;
pub mod executor;
pub mod graph;
pub mod llm_node;
pub mod node;
pub mod state;
pub mod statekey;
pub mod tool_node;

// ─── Error Types ────────────────────────────────────────────────
pub use error::{
    BuildError, BuildErrors, GraphError, ObservedError, RecoverableError, TerminalError,
};

// ─── Events ─────────────────────────────────────────────────────
pub use event::{
    BarrierDecision, BarrierId, GraphEvent, GraphExecution, GraphHandle, GraphStream, NodeEvent,
    SpanId, TraceId,
};

// ─── Graph ──────────────────────────────────────────────────────
pub use graph::{CycleAnalysis, Edge, EdgeAnalysis, Graph, GraphBuilder};

// ─── Nodes ──────────────────────────────────────────────────────
pub use node::{
    AgentNode, BarrierDefaultAction, BarrierNode, ConditionNode, ConditionNodeBuilder, GraphNode,
    LLMNode, NextStep, NodeKind, TaskNode, ToolNode,
};

// ─── State ──────────────────────────────────────────────────────
pub use state::{
    ExecutionEntry, GraphResult, State, StateError, StateExt, StateReducer, array_reducer,
};

// ─── StateKey (编译期类型安全) ──────────────────────────────────
pub use statekey::{SK_COUNT, SK_MESSAGES, SK_STEPS, StateKey, StateKeyExt};

// ─── Executor ───────────────────────────────────────────────────
pub use executor::GraphExecutor;
