//! Agent Loop — LLM ↔ 工具调用闭环。
//!
//! 负责 LLM 返回 tool_calls → 执行工具 → 结果注入 → 再次调用 LLM 的循环，
//! 直到 LLM 返回纯文本或达到最大轮次。
//!
//! # 架构分层
//!
//! ```text
//! ToolUseLoop (薄 Facade)
//! ├── graph:  Graph<AgentState>  (预构建的 ReAct Graph)
//! └── config: ToolUseConfig      (构建 ExecutionContext 的默认参数)
//!
//! AgentBuilder::build()   → Arc<Graph<AgentState>>   (DSL 层)
//! AgentBuilder::compile() → ToolUseLoop              (Facade 层)
//! ```

use lellm_core::{ChatResponse, LlmError, Message};
use lellm_graph::Graph;
use std::sync::Arc;

use super::config::{ToolUseConfig, build_request_messages_inner, empty_response};
use super::event::{AgentEvent, AgentStream, StopReason};
use super::tools::ToolSnapshot;
use super::typed_state::{AgentState, AgentStateMerge};

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

/// 薄 Facade — 持有预构建的 Graph，提供便捷执行 API。
///
/// 不是独立的运行时，只是 Graph 的便捷包装。
/// 内部调用 `Graph::run_inline()` / `Graph::run_stream()`，不创建独立的 ExecutionEngine。
///
/// # 架构
///
/// ```text
/// AgentBuilder::build()   → Arc<Graph<AgentState>>   (DSL 层)
/// AgentBuilder::compile() → ToolUseLoop              (Facade 层)
///
/// ToolUseLoop {
///     graph: Arc<Graph<AgentState>>,  // 共享的 ReAct Graph
///     config: ToolUseConfig,          // 构建 ExecutionContext 的默认参数
/// }
/// ```
///
/// # 示例
///
/// ```ignore
/// let agent = AgentBuilder::new(model).tools([...]).compile();
/// let result = agent.invoke(messages).await?;
/// println!("Answer: {}", result.response.text());
/// ```
#[derive(Clone)]
pub struct ToolUseLoop {
    graph: Arc<Graph<AgentState, AgentStateMerge>>,
    config: ToolUseConfig,
}

impl ToolUseLoop {
    /// 从预构建的 Graph 创建 Facade。
    pub fn new(graph: Arc<Graph<AgentState, AgentStateMerge>>, config: ToolUseConfig) -> Self {
        Self { graph, config }
    }

    /// 获取 Graph 引用。
    pub fn graph(&self) -> &Graph<AgentState, AgentStateMerge> {
        &self.graph
    }

    /// 获取配置引用。
    pub fn config(&self) -> &ToolUseConfig {
        &self.config
    }

    /// 非流式执行 — 执行 Agent 循环并返回结果。
    ///
    /// 内部使用预构建的 Graph，调用 `run_inline()` 执行 ReAct 循环。
    ///
    /// # 示例
    /// ```ignore
    /// let agent = AgentBuilder::new(model).tools([...]).compile();
    /// let result = agent.invoke(messages).await?;
    /// println!("Answer: {}", result.response.text());
    /// ```
    pub async fn invoke(&self, messages: Vec<Message>) -> Result<ToolUseResult, LlmError> {
        let initial_messages = build_request_messages_inner(&self.config, &messages)?;

        // 每轮 ReAct 迭代最坏 4 steps: budget_check + llm + post_llm_check + tool
        // N 轮最坏: 4*(N-1) + 3 = 4N-1 (最后一轮无 tool)
        // +1 buffer 应对 edge cases
        let max_steps = self.config.max_iterations * 4 + 1;

        // 初始化 AgentState
        let mut agent_state_init = super::typed_state::AgentState::from_messages(initial_messages);

        // 创建 ExecutionContext<AgentState> 并调用 run_inline
        // ToolUseLoop 不需要自动 checkpoint，传 None
        let mut exec_ctx = lellm_graph::node_context::ExecutionContext::new(
            &mut agent_state_init,
            None,
            lellm_graph::CancellationToken::new(),
            None,
        );

        self.graph
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
    /// 内部使用预构建的 Graph，调用 `run_inline()` + `AgentEventSink`。
    ///
    /// # 示例
    /// ```ignore
    /// let agent = AgentBuilder::new(model).tools([...]).compile();
    /// let mut stream = agent.invoke_stream(messages);
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
        let graph = self.graph.clone();
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

            // 每轮 ReAct 迭代最坏 4 steps: budget_check + llm + post_llm_check + tool
            let max_steps = config.max_iterations * 4 + 1;

            // 2. 初始化 AgentState
            let mut agent_state = super::typed_state::AgentState::from_messages(initial_messages);

            // 3. 创建 AgentEventSink (StreamChunk → AgentEvent 桥接)
            let event_sink = super::event_bridge::AgentEventSink::new(tx.clone());
            let sink: std::sync::Arc<dyn lellm_graph::StreamSink> = std::sync::Arc::new(event_sink);

            // 4. 创建 ExecutionContext 并调用 run_inline
            // ToolUseLoop 不需要自动 checkpoint，传 None
            let mut exec_ctx = lellm_graph::node_context::ExecutionContext::new(
                &mut agent_state,
                Some(sink),
                lellm_graph::CancellationToken::new(),
                None,
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
