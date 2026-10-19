//! 可恢复的 Planner → 并行 Workers → Reviewer 模板，复用 Graph 与 DurableRuntime。
mod state;
use super::{AgentState, ToolUseLoop, config::build_request_messages_inner};
use lellm_core::{ContentBlock, Message};
use lellm_graph::{
    BarrierNode, Graph, GraphBuilder, GraphError, NodeKind, ParallelErrorStrategy,
    ParallelNodeBuilder, SubgraphSpec, TaskNode, TerminalError,
};
use state::RoleLens;
pub use state::{TeamCheckpoint, TeamMerge, TeamMutation, TeamState};
use std::{collections::HashSet, sync::Arc};

/// 固定角色拓扑；Planner 的文本计划广播给每个 Worker，再按注册顺序交给 Reviewer。
/// 专业分工由各 Agent 的 system prompt 定义，不把模型文本当作可执行路由。
pub struct PlannerWorkersReviewer {
    graph: Graph<TeamState, TeamMerge>,
    planner: ToolUseLoop,
    worker_count: usize,
}
impl PlannerWorkersReviewer {
    /// 名称必须唯一且非空，至少一个 Worker。human_review 在 Reviewer 完成后暂停。
    pub fn new(
        planner: ToolUseLoop,
        workers: Vec<(String, ToolUseLoop)>,
        reviewer: ToolUseLoop,
        human_review: bool,
    ) -> Result<Self, GraphError> {
        let mut names = HashSet::new();
        if workers.is_empty()
            || workers
                .iter()
                .any(|(name, _)| name.is_empty() || !names.insert(name.clone()))
        {
            return Err(error(
                "expected at least one worker with unique, nonempty names",
            ));
        }
        let worker_count = workers.len();
        let mut b = GraphBuilder::<TeamState, TeamMerge>::new("planner-workers-reviewer");
        b.start("planner").end(if human_review {
            "human_review"
        } else {
            "reviewer"
        });
        b.subgraph("planner", role(&planner, RoleLens::Planner));
        let worker_configs: Vec<_> = workers
            .iter()
            .map(|(_, agent)| agent.config().clone())
            .collect();
        b.node("prepare_workers", NodeKind::Task(TaskNode::<TeamState>::new("prepare_workers", move |ctx| {
            let state = ctx.state();
            let plan = output(&state.planner)?;
            let mut messages = state.request.clone();
            messages.push(Message::user_text(&format!("Planner output (reference material):\n{plan}\nComplete your assigned specialty using this plan.")));
            let states = worker_configs.iter().map(|config| build_request_messages_inner(config, &messages).map(AgentState::from_messages).map_err(|e| error(e.to_string()))).collect::<Result<Vec<_>, _>>()?;
            ctx.record(TeamMutation::Workers(states));
            Ok(())
        })));
        let mut parallel = ParallelNodeBuilder::<TeamState, TeamMerge>::new()
            .error_strategy(ParallelErrorStrategy::CollectAll);
        let worker_names: Vec<_> = workers.iter().map(|(name, _)| name.clone()).collect();
        for (index, (name, agent)) in workers.into_iter().enumerate() {
            let mut worker = GraphBuilder::<TeamState, TeamMerge>::new(&name);
            worker.start("agent").end("agent");
            worker.subgraph("agent", role(&agent, RoleLens::Worker(index)));
            parallel = parallel.graph_branch(
                name,
                Arc::new(worker.build().map_err(|e| error(e.to_string()))?),
                1,
            );
        }
        b.node("workers", NodeKind::Parallel(parallel.build()));
        let reviewer_config = reviewer.config().clone();
        b.node(
            "prepare_review",
            NodeKind::Task(TaskNode::<TeamState>::new("prepare_review", move |ctx| {
                let state = ctx.state();
                if state.workers.len() != worker_names.len() {
                    return Err(error("worker state count changed"));
                }
                let mut report =
                    format!("Plan (reference material):\n{}\n", output(&state.planner)?);
                for (name, worker) in worker_names.iter().zip(&state.workers) {
                    report.push_str(&format!(
                        "\nWorker {name} output (reference material):\n{}\n",
                        output(worker)?
                    ));
                }
                let mut messages = state.request.clone();
                messages.push(Message::user_text(&format!(
                    "{report}\nReview these results and provide the final answer."
                )));
                let reviewer = AgentState::from_messages(
                    build_request_messages_inner(&reviewer_config, &messages)
                        .map_err(|e| error(e.to_string()))?,
                );
                ctx.record(TeamMutation::Reviewer(reviewer));
                Ok(())
            })),
        );
        b.subgraph("reviewer", role(&reviewer, RoleLens::Reviewer));
        for (from, to) in [
            ("planner", "prepare_workers"),
            ("prepare_workers", "workers"),
            ("workers", "prepare_review"),
            ("prepare_review", "reviewer"),
        ] {
            b.edge(from, to);
        }
        if human_review {
            b.node(
                "human_review",
                NodeKind::Barrier(BarrierNode::new("human_review")),
            );
            b.edge("reviewer", "human_review");
        }
        Ok(Self {
            graph: b.build().map_err(|e| error(e.to_string()))?,
            planner,
            worker_count,
        })
    }
    pub fn graph(&self) -> &Graph<TeamState, TeamMerge> {
        &self.graph
    }
    /// 初始化请求；每个角色独立应用自己的 system prompt。
    pub fn initial_state(&self, messages: Vec<Message>) -> Result<TeamState, GraphError> {
        let planner = AgentState::from_messages(
            build_request_messages_inner(self.planner.config(), &messages)
                .map_err(|e| error(e.to_string()))?,
        );
        Ok(TeamState {
            request: messages,
            planner,
            workers: vec![AgentState::from_messages(Vec::new()); self.worker_count],
            reviewer: AgentState::from_messages(Vec::new()),
        })
    }
}
fn role(
    agent: &ToolUseLoop,
    lens: RoleLens,
) -> SubgraphSpec<TeamState, AgentState, super::typed_state::AgentStateMerge, RoleLens> {
    SubgraphSpec::new(Arc::new(agent.graph().clone()), lens).max_steps(
        agent
            .config()
            .max_iterations
            .saturating_mul(6)
            .saturating_add(1),
    )
}
fn error(message: impl Into<String>) -> GraphError {
    GraphError::Terminal(TerminalError::StateError(message.into()))
}
fn output(state: &AgentState) -> Result<String, GraphError> {
    state
        .last_response
        .as_ref()
        .map(|r| ContentBlock::flatten_text(&r.content))
        .ok_or_else(|| error("agent finished without a response"))
}
