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

pub use error::GraphError;
pub use event::{BarrierDecision, GraphEvent, GraphStream};
pub use executor::GraphExecutor;
pub use graph::{Edge, Graph, GraphBuilder};
pub use node::{
    AgentNode, BarrierDefaultAction, BarrierNode, ConditionNode, ConditionNodeBuilder, GraphNode,
    LLMNode, LoopNode, NextStep, NodeKind, SubGraph, TaskNode, ToolNode,
};
pub use state::{ExecutionEntry, GraphResult, State, StateExt, StateReducer, array_reducer};
