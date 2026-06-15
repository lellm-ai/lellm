//! lellm-graph — Graph/Node/Edge 编排层。
//!
//! 提供 Workflow DAG + Loop Node 编排能力。

pub mod error;
pub mod executor;
pub mod graph;
pub mod node;
pub mod state;

pub use error::GraphError;
pub use executor::GraphExecutor;
pub use graph::{Edge, Graph, GraphBuilder};
pub use node::{
    AgentNode, ConditionNode, ConditionNodeBuilder, GraphNode, LoopNode, NextStep, NodeKind,
    SubGraph, TaskNode, ToolNode,
};
pub use state::{ExecutionEntry, GraphResult, State};
