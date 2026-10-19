//! 工具级持久化调度；沿用目录快照、重试策略和工具结果协议。
use super::{BatchExecutionResult, ToolExecutor, ToolSnapshot};
use indexmap::IndexMap;
use lellm_core::{
    Message, ParallelSafety, ToolCall, ToolCategory, ToolExecutionContext, ToolReplayPolicy,
};
use lellm_graph::{
    CancellationToken, GraphError, TerminalError,
    durable::{DurableError, ReplayPolicy, TaskRuntime},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

impl ToolExecutor {
    /// 每个工具保存自己的结果；所有组结束后再将完整消息批次交给 ToolNode。
    pub async fn execute_durable_batch(
        &self,
        calls: &[ToolCall],
        tasks: TaskRuntime,
        cancel: CancellationToken,
    ) -> Result<BatchExecutionResult, GraphError> {
        let snapshot = self.snapshot().await;
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut categories: IndexMap<ToolCategory, Vec<usize>> = IndexMap::new();
        let mut exclusive = Vec::new();
        for (i, call) in calls.iter().enumerate() {
            let tool = snapshot.get(&call.name);
            match tool.map(|t| &t.safety) {
                Some(ParallelSafety::Safe) => groups.push(vec![i]),
                Some(ParallelSafety::CategoryExclusive) => {
                    if let Some(category) = tool.and_then(|t| t.category.clone()) {
                        categories.entry(category).or_default().push(i);
                    } else {
                        exclusive.push(i);
                    }
                }
                _ => exclusive.push(i),
            }
        }
        groups.extend(categories.into_values());
        let run_group = |indices: Vec<usize>| {
            let snapshot = snapshot.clone();
            let tasks = tasks.clone();
            let cancel = cancel.clone();
            async move {
                let mut results = Vec::new();
                for i in indices {
                    let start = Instant::now();
                    let message = self
                        .durable_one(&calls[i], i, &snapshot, &tasks, &cancel)
                        .await?;
                    results.push((i, message, start.elapsed()));
                }
                Ok::<_, GraphError>(results)
            }
        };
        // 不 spawn 脱离生命周期的任务；返回前收齐所有组及其持久化结果。
        let parallel = futures_util::future::join_all(groups.into_iter().map(run_group)).await;
        let mut completed = Vec::new();
        let mut error = None;
        for result in parallel {
            match result {
                Ok(values) => completed.extend(values),
                Err(e) => {
                    if error.is_none() {
                        error = Some(e);
                    }
                }
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        completed.extend(run_group(exclusive).await?);
        completed.sort_by_key(|(i, _, _)| *i);
        let (results, durations) = completed
            .into_iter()
            .map(|(_, message, duration)| (message, duration))
            .unzip();
        Ok(BatchExecutionResult {
            results,
            durations,
            panicked: false,
        })
    }

    async fn durable_one(
        &self,
        call: &ToolCall,
        index: usize,
        snapshot: &Arc<ToolSnapshot>,
        tasks: &TaskRuntime,
        cancel: &CancellationToken,
    ) -> Result<Message, GraphError> {
        let tool = snapshot
            .get(&call.name)
            .ok_or_else(|| DurableError::Unsupported(format!("unknown tool {}", call.name)))?;
        if tool.replay_policy == ToolReplayPolicy::IdempotentWithKey
            && !tool.supports_execution_context()
        {
            return Err(DurableError::Unsupported(format!(
                "{} declares idempotency without a context executor",
                call.name
            ))
            .into());
        }
        let policy = match tool.replay_policy {
            ToolReplayPolicy::ManualRecovery => ReplayPolicy::ManualRecovery,
            ToolReplayPolicy::ReplaySafe => ReplayPolicy::ReplaySafe,
            ToolReplayPolicy::IdempotentWithKey => ReplayPolicy::IdempotentWithKey,
        };
        let slot = serde_json::to_string(&(index, &call.id)).unwrap();
        let input = serde_json::json!({ "name": call.name, "arguments": call.arguments });
        tasks
            .execute(
                &slot,
                &tool.replay_version,
                input,
                policy,
                |invocation| async move {
                    let attempt = AtomicU32::new(invocation.attempt);
                    let invoke = || {
                        let cancel = cancel.clone();
                        let context = ToolExecutionContext::new(
                            invocation.task_id.clone(),
                            invocation.idempotency_key.clone(),
                            attempt.fetch_add(1, Ordering::Relaxed),
                        )
                        .with_cancellation(move || cancel.is_cancelled());
                        tool.execute_with_context(&call.arguments, &context)
                    };
                    let result = if policy == ReplayPolicy::ManualRecovery {
                        invoke().await
                    } else {
                        self.retry_policy().execute(invoke).await
                    };
                    if policy == ReplayPolicy::ManualRecovery
                        && let Err(error) = result
                    {
                        // 不能从错误返回值推断外部副作用没有发生；交给对账流程。
                        return Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                            node: call.name.clone(),
                            source: error.to_string().into(),
                        }));
                    }
                    Ok(Message::tool_result(call, &result))
                },
            )
            .await
    }
}
