//! AgentFlowNode — 将 Agent 包装为 Graph FlowNode。
//!
//! 在 Graph 编排中作为节点执行 Agent Loop，读写 State 中的消息。
//!
//! v04: 支持两种执行模式：
//! - **ToolUseLoop 模式**（默认）：直接调用 ToolUseLoop，简单高效
//! - **ReAct Graph 模式**：内部构建 LLM → Condition → Tool → LLM 有环图，架构更清晰

use async_trait::async_trait;

use lellm_graph::{FlowNode, Graph, GraphError, NodeContext, TerminalError};

use crate::hook::{AgentHook, AgentHookContext, AgentHookSnapshot};
use crate::runtime::{AgentEvent, ToolUseLoop, ToolUseResult};

/// Agent 在 Graph 中的节点包装。
///
/// 将 `ToolUseLoop` 适配为 `FlowNode`，使其可以作为 Graph 的节点执行。
///
/// # State 约定
///
/// - 输入: `ctx.get("messages")` → `Vec<serde_json::Value>` 或 `serde_json::Value` 数组
/// - 输出: `ctx.set("messages")` → 更新后的消息列表
/// - 自定义 key: 通过 `message_key` 配置
///
/// # 执行模式
///
/// - **ToolUseLoop 模式**（默认）：直接调用 ToolUseLoop，保留所有功能（context budget、compaction、fallback、retry）
/// - **ReAct Graph 模式**：内部构建 LLM → Condition → Tool → LLM 有环图，架构更清晰
///
/// # 示例
///
/// ```rust,ignore
/// use lellm_agent::AgentFlowNode;
/// use lellm_graph::{GraphBuilder, NodeKind};
///
/// // ToolUseLoop 模式（默认）
/// let agent = AgentFlowNode::new("agent", tool_use_loop.clone());
/// let mut graph = GraphBuilder::new("my_graph");
/// graph.node("agent", NodeKind::External(Arc::new(agent)));
///
/// // ReAct Graph 模式
/// let agent = AgentFlowNode::new("agent", tool_use_loop)
///     .use_react_graph(true);
/// graph.node("agent_react", NodeKind::External(Arc::new(agent)));
/// ```
#[derive(Clone)]
pub struct AgentFlowNode {
    /// 节点名称
    name: String,
    /// Agent 执行循环
    loop_: ToolUseLoop,
    /// State 中消息的 key（默认 "messages"）
    message_key: String,
    /// 是否使用 ReAct Graph 模式（默认 false）
    use_react_graph: bool,
    /// Agent-level hooks（在 agent loop 前后调用）
    hooks: Vec<std::sync::Arc<dyn AgentHook>>,
}

impl AgentFlowNode {
    /// 创建新的 AgentFlowNode。
    pub fn new(name: impl Into<String>, loop_: ToolUseLoop) -> Self {
        Self {
            name: name.into(),
            loop_,
            message_key: "messages".to_string(),
            use_react_graph: false,
            hooks: Vec::new(),
        }
    }

    /// 设置 State 中消息的 key（默认 "messages"）。
    pub fn message_key(mut self, key: impl Into<String>) -> Self {
        self.message_key = key.into();
        self
    }

    /// 使用 ReAct Graph 模式（内部构建 LLM → Condition → Tool → LLM 有环图）。
    pub fn use_react_graph(mut self, enabled: bool) -> Self {
        self.use_react_graph = enabled;
        self
    }

    /// 添加 Agent-level hook。
    ///
    /// Hook 在 agent loop 执行前后调用。
    pub fn hook(mut self, hook: impl AgentHook + 'static) -> Self {
        self.hooks.push(std::sync::Arc::new(hook));
        self
    }

    /// 从 State 中提取输入消息。
    fn extract_messages(&self, ctx: &NodeContext<'_>) -> Vec<lellm_core::Message> {
        if let Some(value) = ctx.get_raw(&self.message_key) {
            if let Some(arr) = value.as_array() {
                let mut messages = Vec::new();
                for v in arr {
                    if let Ok(msg) = serde_json::from_value::<lellm_core::Message>(v.clone()) {
                        messages.push(msg);
                    }
                }
                messages
            } else if let Ok(msg) = serde_json::from_value::<lellm_core::Message>(value.clone()) {
                vec![msg]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    /// 将执行结果写入 ctx。
    fn apply_result(&self, ctx: &mut NodeContext<'_>, result: &ToolUseResult) {
        let messages: Vec<serde_json::Value> = result
            .messages
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();

        ctx.set(&self.message_key, serde_json::json!(messages));
        ctx.set(
            format!("{}_stop_reason", self.name),
            format!("{:?}", result.stop_reason),
        );
        ctx.set(
            format!("{}_iterations", self.name),
            result.iterations as u64,
        );
        ctx.set(
            format!("{}_tool_calls", self.name),
            result.tool_calls_executed as u64,
        );
    }

    /// 构建内部 ReAct Graph。
    ///
    /// ```text
    /// START → budget_check
    ///
    /// budget_check --budget_ok--> [llm]
    ///          --need_compact--> [compactor] → [llm]
    ///
    /// [llm] → [tool_decision]
    ///    --has_tool_calls--> [tool] → [budget_check] (循环)
    ///    --no_tool_calls--> [end]
    /// ```
    fn build_react_graph(&self) -> Graph {
        let config = self.loop_.config().clone();
        let model = self.loop_.model().clone();
        let executor = self.loop_.executor().clone();
        let deps = self.loop_.deps().clone();

        let llm_node = crate::runtime::react::LLMNode::new(
            format!("{}_llm", self.name),
            model,
            executor.clone(),
            config.clone(),
            deps,
        );

        let tool_node = crate::runtime::react::ToolNode::new(
            format!("{}_tool", self.name),
            executor.clone(),
            config.clone(),
        );

        let compactor_node = crate::runtime::react::CompactorNode::new(
            format!("{}_compactor", self.name),
            Box::new(crate::runtime::LocalCompactor::new()),
            config.context_budget.clone(),
        );

        crate::runtime::react::build_react_graph(llm_node, tool_node, compactor_node)
    }
}

#[async_trait]
impl FlowNode for AgentFlowNode {
    /// 执行 — 运行完整的 Agent Loop。
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let messages = self.extract_messages(ctx);

        // 如果没有消息，发送一个警告但仍继续执行
        if messages.is_empty() {
            tracing::debug!(
                agent = %self.name,
                "no input messages found in state key '{}'",
                self.message_key
            );
        }

        // 调用 before_agent hooks
        let hook_ctx = AgentHookContext {
            node_name: self.name.clone(),
            input_message_count: messages.len(),
        };
        for hook in &self.hooks {
            hook.before_agent(&hook_ctx);
        }

        if self.use_react_graph {
            // ReAct Graph 模式：构建内部有环图并执行
            self.execute_with_react_graph(ctx, messages).await
        } else {
            // ToolUseLoop 模式：直接调用 ToolUseLoop
            self.execute_with_tool_use_loop(ctx, messages).await
        }
    }
}

impl AgentFlowNode {
    /// 使用 ToolUseLoop 模式执行。
    async fn execute_with_tool_use_loop(
        &self,
        ctx: &mut NodeContext<'_>,
        messages: Vec<lellm_core::Message>,
    ) -> Result<(), GraphError> {
        // 启动流式 Agent Loop 收集结果
        let mut agent_stream = self.loop_.execute_stream(messages);
        let mut final_result: Option<ToolUseResult> = None;
        let mut error: Option<Box<dyn std::error::Error + Send + Sync>> = None;
        let mut events: Vec<AgentEvent> = Vec::new();

        while let Some(agent_event) = agent_stream.recv().await {
            let is_terminal = matches!(
                &agent_event,
                AgentEvent::LoopEnd { .. } | AgentEvent::LoopError { .. }
            );

            events.push(agent_event.clone());

            // 转发流式事件到 ctx.emit()
            match &agent_event {
                AgentEvent::Provider(provider_event) => match provider_event {
                    lellm_provider::ProviderEvent::Token { token } => {
                        ctx.emit(lellm_graph::StreamChunk::Text(token.clone()));
                    }
                    lellm_provider::ProviderEvent::ThinkingDelta { thinking, .. } => {
                        ctx.emit(lellm_graph::StreamChunk::Thinking(thinking.clone()));
                    }
                    _ => {}
                },
                _ => {}
            }

            if is_terminal {
                match &agent_event {
                    AgentEvent::LoopEnd { result } => {
                        final_result = Some(result.clone());
                    }
                    AgentEvent::LoopError { error: err, .. } => {
                        error = Some(Box::new(err.clone()));
                    }
                    _ => {}
                }
            }
        }

        // 处理错误
        if let Some(err) = error {
            return Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: self.name.clone(),
                source: err,
            }));
        }

        // 写入最终结果
        if let Some(result) = final_result {
            // 调用 after_agent hooks
            let snapshot = AgentHookSnapshot {
                result: result.clone(),
                events,
            };
            for hook in &self.hooks {
                hook.after_agent(&snapshot);
            }

            self.apply_result(ctx, &result);

            tracing::debug!(
                agent = %self.name,
                iterations = result.iterations,
                tool_calls = result.tool_calls_executed,
                stop_reason = ?result.stop_reason,
                "agent execution completed (ToolUseLoop mode)"
            );
        } else {
            return Err(GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: self.name.clone(),
                source: "agent stream ended without terminal event".into(),
            }));
        }

        Ok(())
    }

    /// 使用 ReAct Graph 模式执行。
    ///
    /// v0.4+ Effect 模式：
    /// 1. 维护 AgentState 生命周期
    /// 2. 节点只 emit_effect，不直接改状态
    /// 3. 循环消费 Effect → apply → 同步到 ctx（供边条件使用）
    async fn execute_with_react_graph(
        &self,
        ctx: &mut NodeContext<'_>,
        messages: Vec<lellm_core::Message>,
    ) -> Result<(), GraphError> {
        use lellm_graph::NextAction;

        // 1. 初始化 Typed State
        let mut agent_state = super::typed_state::AgentState::from_messages(messages);

        // 2. 构建内部 ReAct Graph
        let graph = self.build_react_graph();
        let max_steps = self.loop_.config().max_iterations * 2 + 1;

        // 3. Effect 驱动的执行循环
        let mut current = graph.start_node().to_string();
        let mut step: usize = 0;

        loop {
            step += 1;
            if step > max_steps {
                return Err(GraphError::Terminal(
                    TerminalError::StepsExceeded { limit: max_steps },
                ));
            }

            // 3a. 写入 AgentState 到 ctx（节点可读）
            ctx.set_state(super::typed_state::AGENT_STATE_KEY, agent_state.clone());

            // 3b. 创建子 NodeContext（fork BranchState + 透传 StreamEmitter）
            let mut branch = ctx.state().fork();
            let stream = ctx.stream();
            let mut child_ctx = NodeContext::new(&mut branch, stream);

            // 3c. 写入 AgentState 到子 ctx
            child_ctx.set_state(super::typed_state::AGENT_STATE_KEY, agent_state.clone());

            // 3d. 查找并执行节点
            let node_ref = graph
                .node_map()
                .get(&current)
                .ok_or_else(|| {
                    GraphError::Terminal(TerminalError::NodeNotFound(current.clone()))
                })?;
            node_ref.execute(&mut child_ctx).await?;

            // 3e. 消费 Effect → apply 到 AgentState
            for v in child_ctx.consume_effects() {
                agent_state.apply_from_value(v).map_err(|e| {
                    GraphError::Terminal(TerminalError::NodeExecutionFailed {
                        node: current.clone(),
                        source: Box::new(e),
                    })
                })?;
            }

            // 3f. 提取控制信号
            let (next_action, _signal) = child_ctx.take_control();

            // 3g. 释放 child_ctx 对 branch 的借用
            drop(child_ctx);

            // 3h. 将 BranchState 变更 apply 回父 ctx
            for change in branch.changes() {
                match change.operation {
                    lellm_graph::ChangeOperation::Put => {
                        ctx.set(&change.key, change.value.clone());
                    }
                    lellm_graph::ChangeOperation::Delete => {
                        ctx.remove(&change.key);
                    }
                }
            }

            // 3h. 处理路由
            match next_action {
                NextAction::End => break,
                NextAction::Goto(target) => {
                    current = target;
                }
                NextAction::Next => {
                    if current == graph.end_node() {
                        break;
                    }
                    // 同步 AgentState 关键字段到 ctx（供边条件使用）
                    sync_agent_state_to_ctx(ctx, &agent_state);
                    // 从 ctx 的 BranchState 获取完整 State 用于路由
                    let full_state = ctx.state().to_state();
                    current = graph.resolve_next(&current, &full_state).ok_or_else(|| {
                        GraphError::Terminal(TerminalError::InvalidGraph(format!(
                            "node '{}' has no matching outgoing edge",
                            current
                        )))
                    })?;
                }
            }
        }

        // 4. 从 Typed State 提取结果，传播到外层 State
        if let Some(ref stop_reason) = agent_state.stop_reason {
            ctx.set(
                format!("{}_stop_reason", self.name),
                format!("{:?}", stop_reason),
            );
        }
        ctx.set(
            format!("{}_iterations", self.name),
            agent_state.iterations as u64,
        );
        ctx.set(
            format!("{}_tool_calls", self.name),
            agent_state.total_tool_calls as u64,
        );

        tracing::debug!(
            agent = %self.name,
            iterations = agent_state.iterations,
            tool_calls = agent_state.total_tool_calls,
            "agent execution completed (ReAct Graph mode, Effect-driven)"
        );

        Ok(())
    }
}

/// 同步 AgentState 关键字段到 ctx（供边条件使用）。
///
/// 边条件（`edge_if`）直接读取 `State`（HashMap），不感知 AgentState。
/// 此函数桥接 typed state → dynamic state。
fn sync_agent_state_to_ctx(ctx: &mut NodeContext<'_>, state: &super::typed_state::AgentState) {
    use crate::runtime::react::{
        SK_COMPACT_COUNT, SK_ITERATIONS, SK_OUTPUT_TOKENS, SK_REASONING_TOKENS, SK_TOTAL_TOOL_CALLS,
    };
    ctx.set(SK_ITERATIONS, state.iterations as u64);
    ctx.set(SK_TOTAL_TOOL_CALLS, state.total_tool_calls as u64);
    ctx.set(SK_OUTPUT_TOKENS, state.output_tokens as u64);
    ctx.set(SK_REASONING_TOKENS, state.reasoning_tokens as u64);
    ctx.set(SK_COMPACT_COUNT, state.compact_count as u64);
}
