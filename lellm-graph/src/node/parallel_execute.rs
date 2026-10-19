use crate::durable::{DurableError, NodeScope};
use crate::{
    ExecutionEngine, ExecutorState, GraphBuilder, GraphError, MergeStrategy, NodeKind,
    NoopStepCallback, ParallelErrorStrategy, ParallelNode, TerminalError, WorkflowState,
};
use futures::{StreamExt, stream::FuturesUnordered};

/// 每个分支也是一次单节点图调用，从而复用相同的保存、恢复与预算协议。
#[allow(deprecated)]
pub(crate) async fn execute<S: WorkflowState, M: MergeStrategy<S>>(
    parallel: &ParallelNode<S, M>,
    engine: &mut ExecutionEngine<'_, S>,
) -> Result<(), GraphError> {
    let base = engine.clone_state();
    let group_cancel = engine.cancel_token().child_token();
    let stream = engine.stream_sink();
    let parent = NodeScope::current();
    let mut pending = FuturesUnordered::new();
    for (index, (name, node)) in parallel.branches_iter().enumerate() {
        let name = name.to_owned();
        let node = node.clone();
        let mut state = base.clone();
        let cancel = group_cancel.child_token();
        let stream = stream.clone();
        let scope = parent
            .as_ref()
            .map(|p| p.child(&serde_json::to_string(&(index, &name)).unwrap()));
        pending.push(async move {
            let mut builder = GraphBuilder::<S, M>::new("parallel_branch");
            builder.start("body").end("body");
            builder.node("body", NodeKind::External(node));
            let graph = builder.build().expect("single node branch");
            let result = {
                let mut engine = ExecutionEngine::new(&mut state, stream, cancel, None, None);
                let mut callback = NoopStepCallback;
                // Box 切断 Graph → Parallel → Graph 的递归 Future 类型。
                let run = Box::pin(graph.run_inline(&mut engine, 1, &mut callback));
                if let Some(scope) = scope {
                    scope.within(run).await
                } else {
                    run.await
                }
            };
            (index, name, result.map(|()| state))
        });
    }
    let mut states = Vec::new();
    let mut errors = Vec::new();
    let mut suspended = false;
    let mut stopped = false;
    while let Some((index, name, result)) = pending.next().await {
        match result {
            Ok(state) => states.push((index, state)),
            Err(GraphError::Terminal(TerminalError::Durable(DurableError::Suspended))) => {
                suspended = true
            }
            Err(GraphError::Terminal(TerminalError::Durable(DurableError::Stopped))) => {
                stopped = true;
                group_cancel.cancel();
            }
            Err(error) => {
                // 先记录原始失败，再取消并等待所有兄弟退出，避免重叠恢复。
                errors.push((name, error.to_string()));
                if parallel.error_strategy() == ParallelErrorStrategy::FailFast {
                    group_cancel.cancel();
                }
            }
        }
    }
    if stopped {
        return Err(DurableError::Stopped.into());
    }
    if !errors.is_empty() {
        if parallel.error_strategy() == ParallelErrorStrategy::CollectAll {
            return Err(GraphError::Terminal(TerminalError::ParallelFailures {
                errors,
            }));
        }
        let (node, error) = errors.remove(0);
        return Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
            node,
            source: error.into(),
        }));
    }
    if suspended {
        return Err(DurableError::Suspended.into());
    }
    states.sort_by_key(|(index, _)| *index);
    let merged =
        M::merge(&base, states.into_iter().map(|(_, state)| state).collect()).map_err(|e| {
            GraphError::Terminal(TerminalError::StateError(format!(
                "parallel merge conflict: {e}"
            )))
        })?;
    engine.replace_state(merged);
    Ok(())
}
