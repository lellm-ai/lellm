use super::super::{AgentState, typed_state::AgentCheckpoint};
use lellm_core::Message;
use lellm_graph::{MergeStrategy, StateLens, WorkflowError, WorkflowState};
use serde::{Deserialize, Serialize};

/// 每个角色独立保存 Agent 状态，Worker 不共享消息历史。
#[derive(Debug, Clone)]
pub struct TeamState {
    pub(crate) request: Vec<Message>,
    pub(crate) planner: AgentState,
    pub(crate) workers: Vec<AgentState>,
    pub(crate) reviewer: AgentState,
}
impl TeamState {
    pub fn planner(&self) -> &AgentState {
        &self.planner
    }
    pub fn workers(&self) -> &[AgentState] {
        &self.workers
    }
    pub fn reviewer(&self) -> &AgentState {
        &self.reviewer
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamCheckpoint {
    request: Vec<Message>,
    planner: AgentCheckpoint,
    workers: Vec<AgentCheckpoint>,
    reviewer: AgentCheckpoint,
}
#[derive(Debug)]
pub enum TeamMutation {
    Workers(Vec<AgentState>),
    Reviewer(AgentState),
}
impl lellm_graph::state::workflow_state::StateMutation<TeamState> for TeamMutation {
    fn apply(self, state: &mut TeamState) {
        match self {
            Self::Workers(workers) => state.workers = workers,
            Self::Reviewer(reviewer) => state.reviewer = reviewer,
        }
    }
}
impl WorkflowState for TeamState {
    type Checkpoint = TeamCheckpoint;
    type Mutation = TeamMutation;
    fn snapshot(&self) -> TeamCheckpoint {
        TeamCheckpoint {
            request: self.request.clone(),
            planner: self.planner.snapshot(),
            workers: self.workers.iter().map(WorkflowState::snapshot).collect(),
            reviewer: self.reviewer.snapshot(),
        }
    }
    fn restore(c: TeamCheckpoint) -> Self {
        Self {
            request: c.request,
            planner: AgentState::restore(c.planner),
            workers: c.workers.into_iter().map(AgentState::restore).collect(),
            reviewer: AgentState::restore(c.reviewer),
        }
    }
}

/// Worker 分支只允许改变自己的槽位；重复写入或修改其他角色时拒绝合并。
#[derive(Clone)]
pub struct TeamMerge;
impl MergeStrategy<TeamState> for TeamMerge {
    fn default_instance() -> Self {
        Self
    }
    fn merge(base: &TeamState, branches: Vec<TeamState>) -> Result<TeamState, WorkflowError> {
        let mut result = base.clone();
        let base_json = serde_json::to_value(base.snapshot())?;
        let mut written = vec![false; base.workers.len()];
        for branch in branches {
            let json = serde_json::to_value(branch.snapshot())?;
            if branch.workers.len() != base.workers.len()
                || ["request", "planner", "reviewer"]
                    .iter()
                    .any(|k| json[k] != base_json[k])
            {
                return Err(WorkflowError::MergeConflict(
                    "worker changed team structure or another role".into(),
                ));
            }
            for (i, worker) in branch.workers.into_iter().enumerate() {
                if json["workers"][i] != base_json["workers"][i] {
                    if written[i] {
                        return Err(WorkflowError::MergeConflict(format!(
                            "multiple writers for worker {i}"
                        )));
                    }
                    written[i] = true;
                    result.workers[i] = worker;
                }
            }
        }
        Ok(result)
    }
}

pub(super) enum RoleLens {
    Planner,
    Worker(usize),
    Reviewer,
}
impl StateLens<TeamState, AgentState> for RoleLens {
    fn get<'a>(&self, state: &'a mut TeamState) -> &'a mut AgentState {
        match self {
            Self::Planner => &mut state.planner,
            Self::Worker(i) => &mut state.workers[*i],
            Self::Reviewer => &mut state.reviewer,
        }
    }
}
