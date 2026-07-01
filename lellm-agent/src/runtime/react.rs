//! ReAct Graph — ToolUseLoop 内部构建的有环图。
//!
//! v04 设计：ToolUseLoop 内部不再手写 while 循环，
//! 构建内部 Graph（LLM Node → Condition → Tool Node → 自环），
//! 调用 `Graph::run_inline()` 驱动循环。
//!
//! v0.4+ Typed State: 节点使用 `AgentState` 替代 `HashMap<String, Value>`，
//! 通过 `AgentMutation` 描述状态转换。
//!
//! ```text
//! [LLM] --有tool_calls--> [Tool] --(自环)--> [LLM]
//!      --无tool_calls--> [End]
//! ```

mod graph_builder;
mod guards;
mod llm_node;
mod tool_node;

pub(crate) use graph_builder::build_react_graph;
pub use guards::{BudgetCondition, CompactorNode, PostLLMGuard, StopConfig};
pub use llm_node::LLMNode;
pub use tool_node::ToolNode;
