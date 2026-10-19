use crate::{
    ExecutionEngine, Graph, GraphError, MergeStrategy, NodeContext, NoopStepCallback, WorkflowState,
};
use async_trait::async_trait;
use std::sync::Arc;

/// 将一个图作为并行分支执行；与 SubgraphSpec 一样继承持久化上下文。
pub(crate) struct GraphBranch<S: WorkflowState, M: MergeStrategy<S>> {
    pub graph: Arc<Graph<S, M>>,
    pub max_steps: usize,
}

#[allow(deprecated)]
#[async_trait]
impl<S: WorkflowState, M: MergeStrategy<S>> crate::FlowNode<S> for GraphBranch<S, M> {
    fn validate_approval_handlers(&self) -> Result<(), GraphError> {
        self.graph.validate_approval_handlers(false)
    }
    fn validate_durable(&self) -> Result<(), GraphError> {
        self.graph.validate_durable()
    }
    fn durable_signature(&self) -> Option<String> {
        Some(format!(
            "{}:{}",
            self.max_steps,
            crate::durable::scope_signature(&self.graph)
        ))
    }

    async fn execute(&self, ctx: &mut NodeContext<'_, S>) -> Result<(), GraphError> {
        let mut state = ctx.state().clone();
        let mut engine = ExecutionEngine::new(
            &mut state,
            ctx.stream_sink(),
            ctx.cancel_token().clone(),
            None,
            None,
        );
        let mut cb = NoopStepCallback;
        if let Some(parent) = crate::durable::NodeScope::current() {
            parent
                .child("graph")
                .within(self.graph.run_inline(&mut engine, self.max_steps, &mut cb))
                .await?;
        } else {
            self.graph
                .run_inline(&mut engine, self.max_steps, &mut cb)
                .await?;
        }
        ctx.replace_state(state);
        Ok(())
    }
}
