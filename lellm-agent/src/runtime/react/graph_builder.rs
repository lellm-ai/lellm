//! ReAct Graph 构建 — 组装内部有环图。
//!
//! ```text
//! START → budget_check
//!
//! budget_check --budget_ok--> [llm]
//!          --need_compact--> [compactor] → [llm]
//!
//! [llm] → [post_llm_check]
//!    --budget_exceeded--> [end]
//!    --has_tool_calls--> [tool] → [budget_check] (循环)
//!    --no_tool_calls--> [end]
//! ```

use std::sync::Arc;

use lellm_graph::{Graph, GraphBuilder, NodeKind, TaskNode};

use super::guards::{BudgetCondition, CompactorNode, PostLLMGuard, StopConfig};
use super::llm_node::LLMNode;
use super::tool_node::ToolNode;
use super::super::typed_state::{AgentState, AgentStateMerge};

/// 构建 ReAct 内部图。
///
/// 使用 `Graph<AgentState>` — 节点直接读写强类型 AgentState，零序列化。
pub fn build_react_graph(
    llm_node: LLMNode,
    tool_node: ToolNode,
    compactor_node: CompactorNode,
) -> Graph<AgentState, AgentStateMerge> {
    let llm_name = llm_node.name.clone();
    let budget = llm_node.config.context_budget.clone();
    let stop_config = StopConfig::from_tool_use_config(&llm_node.config);

    let mut builder =
        GraphBuilder::<AgentState, AgentStateMerge>::new(format!("react_{}", llm_name));
    builder.start("budget_check");
    builder.end("end");

    // 节点注册
    builder.node("llm", NodeKind::External(Arc::new(llm_node)));
    builder.node("tool", NodeKind::External(Arc::new(tool_node)));
    builder.node(
        "post_llm_check",
        NodeKind::External(Arc::new(PostLLMGuard::new(
            format!("{}_post_llm", llm_name),
            stop_config,
        ))),
    );
    builder.node(
        "budget_check",
        NodeKind::External(Arc::new(BudgetCondition::new(
            format!("{}_budget", llm_name),
            budget,
        ))),
    );
    builder.node("compactor", NodeKind::External(Arc::new(compactor_node)));
    // End 节点 — no-op 终端节点
    builder.node(
        "end",
        NodeKind::Task(TaskNode::<AgentState>::new("end", |_| Ok(()))),
    );

    // 注意：以下 edges 仅用于静态分析（analyze/diagnostics），运行时不使用。
    // 条件节点通过 ctx.goto()/ctx.end() 控制路由，NextAction::Goto 优先于 edge 解析。
    //
    // 静态边与运行时路由的对应关系：
    //   budget_check → llm          (BudgetCondition: 预算充足时 goto("llm"))
    //   budget_check → compactor    (BudgetCondition: 需要压缩时 goto("compactor"))
    //   compactor → llm             (CompactorNode: 压缩后走下一步，无显式 goto)
    //   llm → post_llm_check        (LLMNode: 调用完走下一步，无显式 goto)
    //   post_llm_check → tool       (PostLLMGuard: 有 tool_calls 时 goto("tool"))
    //   post_llm_check → end        (PostLLMGuard: 无 tool_calls 或 budget 超限时 end())
    //   tool → budget_check         (ToolNode: 执行完走下一步，无显式 goto)
    builder.edge("budget_check", "llm");
    builder.edge_fallback("budget_check", "compactor");
    builder.edge("compactor", "llm");
    builder.edge("llm", "post_llm_check");
    builder.edge("post_llm_check", "tool");
    builder.edge_fallback("post_llm_check", "end");
    builder.edge("tool", "budget_check");

    builder.build().expect("ReAct graph should be valid")
}
