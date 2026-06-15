//! lellm-graph — Graph/Node/Edge 编排层。
//!
//! 提供 Workflow DAG + Loop Node + Human-in-the-loop 编排能力。

pub mod barrier_node;
pub mod error;
pub mod event;
pub mod executor;
pub mod graph;
pub mod llm_node;
pub mod node;
pub mod state;
pub mod tool_node;

// ─── Error Types ────────────────────────────────────────────────
pub use error::{BuildError, GraphError, ObservedError, RecoverableError, TerminalError};

// ─── Events ─────────────────────────────────────────────────────
pub use event::{
    BarrierDecision, BarrierId, BarrierInnerEvent, EventLevel, GraphEvent, GraphExecution,
    GraphHandle, GraphStream, NodeEvent, SpanId, TraceId,
};

// ─── Graph ──────────────────────────────────────────────────────
pub use graph::{
    CycleAnalysis, Edge, EdgeAnalysis, EdgeExceededStrategy, EdgePolicy, Graph, GraphBuilder,
};

// ─── Nodes ──────────────────────────────────────────────────────
pub use node::{
    AgentNode, BarrierDefaultAction, BarrierNode, ConditionNode, ConditionNodeBuilder, GraphNode,
    LLMNode, LoopNode, NextStep, NodeKind, SubGraph, TaskNode, ToolNode,
};

// ─── State ──────────────────────────────────────────────────────
pub use state::{
    ExecutionEntry, GraphResult, State, StateError, StateExt, StateReducer, array_reducer,
};

// ─── Executor ───────────────────────────────────────────────────
pub use executor::GraphExecutor;
