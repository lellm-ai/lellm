use super::*;
use crate::{ExecutionEngine, Graph, GraphError, MergeStrategy, NoopStepCallback, WorkflowState};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 持久化入口；执行仍由 Graph::run_inline 驱动。
pub struct DurableRuntime {
    store: Arc<dyn DurableStore>,
    version: RunVersion,
    observer: Option<RunObserver>,
}

impl DurableRuntime {
    /// 对未知工具结果执行人工对账。重复请求返回成功；冲突结果拒绝。
    pub fn resolve_task(
        &self,
        id: &str,
        task_id: &str,
        request_id: &str,
        expected_revision: u64,
        resolution: TaskResolution,
    ) -> Result<(), DurableError> {
        if request_id.is_empty() {
            return Err(DurableError::Conflict("empty request id".into()));
        }
        let lease = self.store.acquire(id)?;
        let record = lease.load()?.ok_or(DurableError::NotFound)?;
        self.validate_record(id, &record)?;
        let task = record.tasks.get(task_id).ok_or(DurableError::NotFound)?;
        if let Some(previous) = task.resolutions.get(request_id) {
            return if previous == &resolution {
                Ok(())
            } else {
                Err(DurableError::Conflict("resolution changed".into()))
            };
        }
        if record.revision != expected_revision
            || !matches!(record.status, RunStatus::Running | RunStatus::Suspended)
            || !matches!(task.status, TaskStatus::Unknown | TaskStatus::Started)
        {
            return Err(DurableError::Conflict(
                "task/run status or revision changed".into(),
            ));
        }
        FrameScope::new(record, lease).update(|r| {
            let task = r.tasks.get_mut(task_id).unwrap();
            match &resolution {
                TaskResolution::Completed(value) => {
                    task.result = Some(value.clone());
                    task.status = TaskStatus::Completed;
                }
                TaskResolution::Retry => {
                    task.result = None;
                    task.status = TaskStatus::Planned;
                }
            }
            task.resolutions.insert(request_id.into(), resolution);
            Ok(())
        })
    }
    fn validate_record(&self, id: &str, record: &RunRecord) -> Result<(), DurableError> {
        if record.run_id != id || record.version != self.version {
            return Err(DurableError::Incompatible(
                "run identity or workflow/schema version".into(),
            ));
        }
        record.validate()
    }
    pub fn new(store: Arc<dyn DurableStore>, version: RunVersion) -> Self {
        Self {
            store,
            version,
            observer: None,
        }
    }
    /// 回调应快速返回；接收不到事件不会影响持久化控制协议。
    pub fn with_observer(mut self, observer: RunObserver) -> Self {
        self.observer = Some(observer);
        self
    }
    pub fn inspect(&self, id: &str) -> Result<RunRecord, DurableError> {
        self.store.inspect(id)
    }

    pub async fn start<S: WorkflowState, M: MergeStrategy<S>>(
        &self,
        id: &str,
        graph: &Graph<S, M>,
        state: S,
        max_steps: usize,
        cancel: CancellationToken,
    ) -> Result<RunOutcome<S>, GraphError> {
        graph.validate_durable()?;
        if self.version.workflow.is_empty() || self.version.state_schema.is_empty() {
            return Err(DurableError::Incompatible("empty workflow/schema version".into()).into());
        }
        let lease = self.store.acquire(id)?;
        if lease.load()?.is_some() {
            return Err(DurableError::AlreadyExists.into());
        }
        let record = super::scope::root_record(id, self.version.clone(), graph, &state, max_steps)?;
        lease.save(&record)?;
        self.execute(graph, FrameScope::new(record, lease), state, cancel)
            .await
    }

    pub async fn resume<S: WorkflowState, M: MergeStrategy<S>>(
        &self,
        id: &str,
        graph: &Graph<S, M>,
        cancel: CancellationToken,
    ) -> Result<RunOutcome<S>, GraphError> {
        graph.validate_durable()?;
        let lease = self.store.acquire(id)?;
        let record = lease.load()?.ok_or(DurableError::NotFound)?;
        self.validate_record(id, &record)?;
        let root = record.frames.get("root").ok_or(DurableError::NotFound)?;
        if root.graph_signature != super::scope::signature(graph) {
            return Err(DurableError::Incompatible("root graph".into()).into());
        }
        let state =
            S::restore(serde_json::from_value(root.state.clone()).map_err(DurableError::from)?);
        match record.status {
            RunStatus::Completed => return Ok(RunOutcome::Completed(state)),
            RunStatus::Rejected | RunStatus::Cancelled | RunStatus::Expired => {
                return Ok(RunOutcome::Stopped(record.status));
            }
            _ => {}
        }
        self.execute(graph, FrameScope::new(record, lease), state, cancel)
            .await
    }

    async fn execute<S: WorkflowState, M: MergeStrategy<S>>(
        &self,
        graph: &Graph<S, M>,
        mut scope: FrameScope,
        mut state: S,
        cancel: CancellationToken,
    ) -> Result<RunOutcome<S>, GraphError> {
        scope.root_cancel = cancel.clone();
        scope.observer = self.observer.clone();
        let record = scope.record();
        let pending: Vec<_> = record
            .approvals
            .values()
            .filter(|a| !a.applied && a.receipt.is_none())
            .collect();
        if pending.iter().any(|a| {
            a.deadline_unix_ms
                .is_some_and(|t| super::scope::now_ms() >= t)
        }) {
            scope.update(|r| {
                r.status = RunStatus::Expired;
                Ok(())
            })?;
            return Ok(RunOutcome::Stopped(RunStatus::Expired));
        }
        if !cancel.is_cancelled()
            && (!pending.is_empty()
                || record
                    .tasks
                    .values()
                    .any(|t| t.status == TaskStatus::Unknown))
        {
            return Ok(RunOutcome::Suspended(record));
        }
        let max_steps = scope.record().max_steps;
        scope.update(|r| {
            r.status = RunStatus::Running;
            Ok(())
        })?;
        let result = {
            let mut engine = ExecutionEngine::new(&mut state, None, cancel, None, None);
            scope
                .within(graph.run_inline(&mut engine, max_steps, &mut NoopStepCallback))
                .await
        };
        if scope.root_cancel.is_cancelled() {
            scope.update(|r| {
                r.status = RunStatus::Cancelled;
                Ok(())
            })?;
            return Ok(RunOutcome::Stopped(RunStatus::Cancelled));
        }
        match result {
            Ok(()) => {
                scope.update(|r| {
                    r.status = RunStatus::Completed;
                    Ok(())
                })?;
                Ok(RunOutcome::Completed(state))
            }
            Err(GraphError::Terminal(crate::TerminalError::Durable(DurableError::Suspended))) => {
                Ok(RunOutcome::Suspended(scope.record()))
            }
            Err(GraphError::Terminal(crate::TerminalError::Durable(DurableError::Stopped))) => {
                Ok(RunOutcome::Stopped(scope.record().status))
            }
            Err(e) => Err(e),
        }
    }

    pub fn submit_decision(
        &self,
        id: &str,
        approval_id: &str,
        content_token: &str,
        request_id: &str,
        expected_revision: u64,
        decision: ApprovalDecision,
    ) -> Result<DecisionReceipt, DurableError> {
        if request_id.is_empty() {
            return Err(DurableError::Conflict("empty request id".into()));
        }
        let lease = self.store.acquire(id)?;
        let record = lease.load()?.ok_or(DurableError::NotFound)?;
        self.validate_record(id, &record)?;
        let scope = FrameScope::new(record, lease);
        let record = scope.record();
        let request = record
            .approvals
            .get(approval_id)
            .ok_or(DurableError::NotFound)?;
        if request.content_token != content_token {
            return Err(DurableError::Conflict("approval content changed".into()));
        }
        if let Some(receipt) = &request.receipt {
            if receipt.request_id == request_id && receipt.decision == decision {
                return Ok(receipt.clone());
            }
            return Err(DurableError::Conflict("approval already decided".into()));
        }
        if record.status != RunStatus::Suspended || record.revision != expected_revision {
            return Err(DurableError::Conflict(
                "run status or revision changed".into(),
            ));
        }
        if request
            .deadline_unix_ms
            .is_some_and(|t| super::scope::now_ms() >= t)
        {
            scope.update(|r| {
                r.status = RunStatus::Expired;
                Ok(())
            })?;
            return Err(DurableError::Conflict("approval expired".into()));
        }
        let receipt = DecisionReceipt {
            request_id: request_id.into(),
            decision,
            accepted_revision: record
                .revision
                .checked_add(1)
                .ok_or_else(|| DurableError::Conflict("revision overflow".into()))?,
        };
        scope.update(|r| {
            r.approvals.get_mut(approval_id).unwrap().receipt = Some(receipt.clone());
            Ok(())
        })?;
        Ok(receipt)
    }

    pub fn cancel(&self, id: &str, expected_revision: u64) -> Result<(), DurableError> {
        let lease = self.store.acquire(id)?;
        let record = lease.load()?.ok_or(DurableError::NotFound)?;
        self.validate_record(id, &record)?;
        if record.status == RunStatus::Cancelled {
            return Ok(());
        }
        if record.revision != expected_revision
            || !matches!(record.status, RunStatus::Running | RunStatus::Suspended)
        {
            return Err(DurableError::Conflict(
                "run status or revision changed".into(),
            ));
        }
        FrameScope::new(record, lease).update(|r| {
            r.status = RunStatus::Cancelled;
            Ok(())
        })
    }
}
