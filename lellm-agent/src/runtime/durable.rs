use super::{
    AgentState, ToolUseLoop, ToolUseResult, checkpoint::build_result,
    config::build_request_messages_inner,
};
use lellm_core::{LlmError, Message};
use lellm_graph::{
    CancellationToken,
    durable::{DurableRuntime, RunOutcome},
};

fn outcome(value: RunOutcome<AgentState>) -> RunOutcome<ToolUseResult> {
    match value {
        RunOutcome::Completed(state) => RunOutcome::Completed(build_result(&state)),
        RunOutcome::Suspended(record) => RunOutcome::Suspended(record),
        RunOutcome::Stopped(status) => RunOutcome::Stopped(status),
    }
}

impl ToolUseLoop {
    /// 持久化运行。暂停返回可查询的审批/任务记录，不持有后台等待任务。
    pub async fn invoke_durable(
        &self,
        runtime: &DurableRuntime,
        run_id: &str,
        messages: Vec<Message>,
        cancel: CancellationToken,
    ) -> Result<RunOutcome<ToolUseResult>, LlmError> {
        let messages = build_request_messages_inner(self.config(), &messages)?;
        // 包括压缩和审批节点，使用溢出安全的总预算。
        let budget = self
            .config()
            .max_iterations
            .saturating_mul(6)
            .saturating_add(1);
        runtime
            .start(
                run_id,
                self.graph(),
                AgentState::from_messages(messages),
                budget,
                cancel,
            )
            .await
            .map(outcome)
            .map_err(super::checkpoint::map_runtime_error)
    }

    pub async fn resume_durable(
        &self,
        runtime: &DurableRuntime,
        run_id: &str,
        cancel: CancellationToken,
    ) -> Result<RunOutcome<ToolUseResult>, LlmError> {
        runtime
            .resume(run_id, self.graph(), cancel)
            .await
            .map(outcome)
            .map_err(super::checkpoint::map_runtime_error)
    }
}
