//! Agent Loop — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。
//!
//! # 架构分层
//!
//! ```text
//! ToolUseLoop
//! ├── model:       ResolvedModel     (Provider + model name)
//! ├── executor:    ToolExecutor      (ToolCatalog + 执行引擎)
//! ├── config:      ToolUseConfig     (纯参数, Clone + Send + Sync)
//! └── deps:        ToolUseDeps       (策略服务, Arc 包裹)
//! ```

use lellm_core::{ChatResponse, LlmError, Message};
use lellm_provider::ResolvedModel;
use std::sync::Arc;

use super::config::{ToolUseConfig, ToolUseDeps, build_request_messages_inner, empty_response};
use super::context::LocalCompactor;
use super::event::{AgentEvent, AgentStream, StopReason};
use super::tools::{ToolExecutor, ToolSnapshot};

// ─── 本轮解析数据 ────────────────────────────────────────────────

/// 本轮对话锁定的快照 + 定义。
///
/// 一旦创建，内容不再变化。充当单轮的"真理之源"。
#[derive(Clone)]
pub struct ResolvedRound {
    /// 本轮对话锁定的快照
    pub snapshot: Arc<ToolSnapshot>,
    /// 为当前 LLM 供给的工具定义（已在前置阶段从快照中提取并平铺）
    pub definitions: Vec<lellm_core::ToolDefinition>,
}

impl ResolvedRound {
    pub fn new(snapshot: Arc<ToolSnapshot>) -> Self {
        Self {
            definitions: snapshot.definitions().to_vec(),
            snapshot,
        }
    }
}

// ─── 执行结果 ───────────────────────────────────────────────────

/// ToolUseLoop 执行结果
#[derive(Debug, Clone)]
pub struct ToolUseResult {
    pub stop_reason: StopReason,
    pub response: ChatResponse,
    pub messages: Vec<Message>,
    pub iterations: usize,
    pub tool_calls_executed: usize,
}

impl ToolUseResult {
    pub fn is_success(&self) -> bool {
        matches!(self.stop_reason, StopReason::Complete)
    }
}

// ─── ToolUseLoop ────────────────────────────────────────────────

/// 管理 LLM 与工具调用闭环。
///
/// 内部全为 Arc/Clone，clone 为 O(1)，支持并发 execute。
#[derive(Clone)]
pub struct ToolUseLoop {
    model: ResolvedModel,
    executor: ToolExecutor,
    config: ToolUseConfig,
    deps: ToolUseDeps,
}

impl ToolUseLoop {
    pub fn new(
        model: ResolvedModel,
        executor: ToolExecutor,
        config: ToolUseConfig,
        deps: ToolUseDeps,
    ) -> Self {
        if config.stream_thinking {
            let caps = model.provider.capabilities_for(&model.model);
            if !caps.supports_stream_thinking {
                tracing::warn!(
                    provider = %model.provider.provider_id(),
                    model = %model.model,
                    "stream_thinking=true but provider does not support thinking deltas; \
                     reasoning content will only be available in the final response"
                );
            }
        }

        Self {
            model,
            executor,
            config,
            deps,
        }
    }

    /// 便捷构造 — 使用默认配置和依赖。
    pub fn simple(model: ResolvedModel, executor: ToolExecutor) -> Self {
        Self::new(
            model,
            executor,
            ToolUseConfig::default(),
            ToolUseDeps::default(),
        )
    }

    /// 获取模型引用。
    pub fn model(&self) -> &ResolvedModel {
        &self.model
    }

    /// 获取执行器引用。
    pub fn executor(&self) -> &ToolExecutor {
        &self.executor
    }

    /// 获取配置引用。
    pub fn config(&self) -> &ToolUseConfig {
        &self.config
    }

    /// 获取依赖引用。
    pub fn deps(&self) -> &ToolUseDeps {
        &self.deps
    }

    /// 构建 LlmInvoker（共享实例）。
    fn build_invoker(&self) -> Arc<super::invoker::LlmInvoker> {
        Arc::new(super::invoker::LlmInvoker::from_config(
            self.model.clone(),
            &self.config,
            self.deps.fallback.clone(),
        ))
    }

    /// 非流式执行 — 执行 Agent 循环并返回结果。
    ///
    /// 内部构建 `Graph<AgentState>`，调用 `run_inline()` 执行 ReAct 循环。
    ///
    /// # 示例
    /// ```ignore
    /// let loop_ = AgentBuilder::new(model).tools([...]).build_loop();
    /// let result = loop_.invoke(messages).await?;
    /// println!("Answer: {}", result.response.text());
    /// ```
    pub async fn invoke(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let initial_messages = build_request_messages_inner(&self.config, &messages)?;

        // 构建 ReAct Graph (Graph<AgentState, AgentStateMerge>)
        let invoker = self.build_invoker();
        let llm_node =
            super::react::LLMNode::new("llm", invoker, self.executor.clone(), self.config.clone());
        let tool_node =
            super::react::ToolNode::new("tool", self.executor.clone(), self.config.clone());
        let compactor_node = super::react::CompactorNode::new(
            "compactor",
            Arc::new(LocalCompactor::new()),
            self.config.context_budget.clone(),
        );
        let graph = super::react::build_react_graph(llm_node, tool_node, compactor_node);
        // 每轮 ReAct 迭代最坏 4 steps: budget_check + llm + post_llm_check + tool
        // N 轮最坏: 4*(N-1) + 3 = 4N-1 (最后一轮无 tool)
        // +1 buffer 应对 edge cases
        let max_steps = self.config.max_iterations * 4 + 1;

        // 初始化 AgentState
        let agent_state_init = super::typed_state::AgentState::from_messages(initial_messages);

        // 创建 ExecutionContext<AgentState> 并调用 run_inline
        let mut exec_ctx = lellm_graph::node_context::ExecutionContext::new(
            agent_state_init,
            None,
            lellm_graph::CancellationToken::new(),
        );

        graph
            .run_inline(&mut exec_ctx, max_steps)
            .await
            .map_err(|e| lellm_core::LlmError::Provider {
                provider: "react_graph".into(),
                status: None,
                code: None,
                message: e.to_string(),
            })?;

        let agent_state = exec_ctx.state();
        let stop_reason = agent_state
            .stop_reason
            .clone()
            .unwrap_or(StopReason::Complete);
        let last_response = agent_state
            .last_response
            .clone()
            .unwrap_or_else(empty_response);

        Ok(ToolUseResult {
            stop_reason,
            response: last_response,
            messages: agent_state.messages.clone(),
            iterations: agent_state.iterations,
            tool_calls_executed: agent_state.total_tool_calls,
        })
    }

    /// 流式执行，返回事件接收器。
    ///
    /// 内部构建 `Graph<AgentState>`，调用 `run_inline()` + `AgentEventSink`。
    ///
    /// # 示例
    /// ```ignore
    /// let loop_ = AgentBuilder::new(model).tools([...]).build_loop();
    /// let mut stream = loop_.invoke_stream(messages);
    /// while let Some(event) = stream.recv().await {
    ///     match event {
    ///         AgentEvent::LoopEnd { result } => println!("Done: {}", result.response.text()),
    ///         AgentEvent::Provider(e) => print!("{}", e.delta()),
    ///         _ => {}
    ///     }
    /// }
    /// ```
    pub fn invoke_stream(&self, messages: Vec<Message>) -> AgentStream {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let invoker = self.build_invoker();
        let executor = self.executor.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            // 1. 构建初始消息
            let initial_messages = match build_request_messages_inner(&config, &messages) {
                Ok(m) => m,
                Err(e) => {
                    let _ = tx
                        .send(AgentEvent::LoopError {
                            error: e,
                            iterations: 0,
                        })
                        .await;
                    return;
                }
            };

            // 2. 构建 ReAct Graph
            let llm_node =
                super::react::LLMNode::new("llm", invoker, executor.clone(), config.clone());
            let tool_node = super::react::ToolNode::new("tool", executor.clone(), config.clone());
            let compactor_node = super::react::CompactorNode::new(
                "compactor",
                Arc::new(LocalCompactor::new()),
                config.context_budget.clone(),
            );
            let graph = super::react::build_react_graph(llm_node, tool_node, compactor_node);

            // 每轮 ReAct 迭代最坏 4 steps: budget_check + llm + post_llm_check + tool
            let max_steps = config.max_iterations * 4 + 1;

            // 3. 初始化 AgentState
            let agent_state = super::typed_state::AgentState::from_messages(initial_messages);

            // 4. 创建 AgentEventSink (StreamChunk → AgentEvent 桥接)
            let event_sink = super::event_bridge::AgentEventSink::new(tx.clone());
            let sink: std::sync::Arc<dyn lellm_graph::StreamSink> = std::sync::Arc::new(event_sink);

            // 5. 创建 ExecutionContext 并调用 run_inline
            let mut exec_ctx = lellm_graph::node_context::ExecutionContext::new(
                agent_state,
                Some(sink),
                lellm_graph::CancellationToken::new(),
            );

            match graph.run_inline(&mut exec_ctx, max_steps).await {
                Ok(()) => {
                    // 执行成功，从 AgentState 提取结果
                    let state = exec_ctx.state();
                    let stop_reason = state.stop_reason.clone().unwrap_or(StopReason::Complete);
                    let last_response = state.last_response.clone().unwrap_or_else(empty_response);

                    let _ = tx
                        .send(AgentEvent::LoopEnd {
                            result: ToolUseResult {
                                stop_reason,
                                response: last_response,
                                messages: state.messages.clone(),
                                iterations: state.iterations,
                                tool_calls_executed: state.total_tool_calls,
                            },
                        })
                        .await;
                }
                Err(e) => {
                    // 执行失败，发送 LoopError
                    let state = exec_ctx.state();
                    let error = LlmError::Provider {
                        provider: "react_graph".into(),
                        status: None,
                        code: None,
                        message: e.to_string(),
                    };
                    let _ = tx
                        .send(AgentEvent::LoopError {
                            error,
                            iterations: state.iterations,
                        })
                        .await;
                }
            }
        });

        rx
    }
}
