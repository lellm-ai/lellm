//! LLM 相关节点 — AgentNode（完整 ReAct 循环）与 LLMNode（单次调用）。

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::GraphError;
use crate::event::{GraphEvent, NodeEvent, TraceId};
use crate::node::{GraphNode, NextStep, PendingDecisions, StreamNodeResult};
use crate::state::State;

// ─── AgentNode ───────────────────────────────────────────────

/// Agent 节点（包装 ToolUseLoop）。
///
/// 执行后将以下字段写回 State（默认 key 可通过 builder 自定义）：
/// - `{prefix}.messages` — 完整对话历史（含工具调用与结果）
/// - `{prefix}.output` — 最终回复纯文本
/// - `{prefix}.iterations` — LLM 调用轮次
/// - `{prefix}.tool_calls` — 工具调用总数
/// - `{prefix}.stop_reason` — 停止原因（"Complete" / "MaxIterations" / …）
pub struct AgentNode {
    pub name: String,
    pub agent: lellm_agent::ToolUseLoop,
    /// State 中的 key 前缀，默认 "agent"
    pub prefix: String,
    /// 是否写回完整 messages（默认 true）
    pub write_messages: bool,
    /// 是否写回执行统计（默认 true）
    pub write_stats: bool,
}

impl AgentNode {
    pub fn new(name: impl Into<String>, agent: lellm_agent::ToolUseLoop) -> Self {
        Self {
            name: name.into(),
            agent,
            prefix: "agent".into(),
            write_messages: true,
            write_stats: true,
        }
    }

    /// 设置 State key 前缀（默认 "agent"）。
    ///
    /// 写入的 key 为：`{prefix}.messages`、`{prefix}.output` 等。
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// 控制是否将完整对话历史写回 State（默认 true）。
    pub fn with_write_messages(mut self, enabled: bool) -> Self {
        self.write_messages = enabled;
        self
    }

    /// 控制是否写入 iterations / tool_calls / stop_reason（默认 true）。
    pub fn with_write_stats(mut self, enabled: bool) -> Self {
        self.write_stats = enabled;
        self
    }
}

/// 将 StopReason 序列化为简短字符串。
fn stop_reason_str(reason: &lellm_agent::StopReason) -> &'static str {
    match reason {
        lellm_agent::StopReason::Complete => "Complete",
        lellm_agent::StopReason::MaxIterationsReached => "MaxIterations",
        lellm_agent::StopReason::Cancelled => "Cancelled",
        lellm_agent::StopReason::OutputBudgetExceeded => "OutputBudget",
        lellm_agent::StopReason::ReasoningBudgetExceeded => "ReasoningBudget",
    }
}

/// 从 ToolUseResult 写入 State 的公共逻辑。
fn write_agent_result(node: &AgentNode, result: &lellm_agent::ToolUseResult, state: &mut State) {
    // 提取纯文本输出
    let text: String = result
        .response
        .content
        .iter()
        .filter_map(|b| match b {
            lellm_core::ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    if !text.is_empty() {
        state.insert(
            format!("{}.output", node.prefix),
            serde_json::Value::String(text),
        );
    }

    // 写回完整对话历史
    if node.write_messages {
        state.insert(
            format!("{}.messages", node.prefix),
            serde_json::to_value(&result.messages).unwrap_or(serde_json::Value::Null),
        );
    }

    // 写回执行统计
    if node.write_stats {
        state.insert(
            format!("{}.iterations", node.prefix),
            serde_json::json!(result.iterations),
        );
        state.insert(
            format!("{}.tool_calls", node.prefix),
            serde_json::json!(result.tool_calls_executed),
        );
        state.insert(
            format!("{}.stop_reason", node.prefix),
            serde_json::json!(stop_reason_str(&result.stop_reason)),
        );
    }
}

/// 从 State 读取输入消息。
fn read_messages(state: &State, prefix: &str) -> Vec<lellm_core::Message> {
    let input_key = format!("{}.messages", prefix);
    let messages = state
        .get(&input_key)
        .and_then(|v| serde_json::from_value::<Vec<lellm_core::Message>>(v.clone()).ok())
        .unwrap_or_default();

    // 兼容旧 key "messages"
    if messages.is_empty() {
        state
            .get("messages")
            .and_then(|v| serde_json::from_value::<Vec<lellm_core::Message>>(v.clone()).ok())
            .unwrap_or_default()
    } else {
        messages
    }
}

#[async_trait]
impl GraphNode for AgentNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        let messages = read_messages(state, &self.prefix);

        let result =
            self.agent
                .execute(messages)
                .await
                .map_err(|e| GraphError::NodeExecutionFailed {
                    node: self.name.clone(),
                    source: Box::new(e),
                })?;

        write_agent_result(self, &result, state);
        Ok(NextStep::GoToNext)
    }

    /// 流式执行 — 使用 ToolUseLoop::execute_stream，转发 AgentEvent。
    async fn execute_stream(
        &self,
        state: &mut State,
        sink: &mpsc::Sender<GraphEvent>,
        trace_id: TraceId,
        _pending_decisions: PendingDecisions,
    ) -> Result<StreamNodeResult, GraphError> {
        let messages = read_messages(state, &self.prefix);
        let node_name = self.name.clone();

        // 使用 ToolUseLoop 的流式执行
        let mut stream = self.agent.execute_stream(messages);

        /// 从 AgentEvent 中提取终态信息（避免 move 问题）。
        struct ExtractedResult {
            write_result: Option<lellm_agent::ToolUseResult>,
            error_msg: Option<String>,
        }

        // 转发 Agent 事件，等待 LoopEnd 或 LoopError
        while let Some(event) = stream.recv().await {
            let extracted = match &event {
                lellm_agent::AgentEvent::LoopEnd { result } => ExtractedResult {
                    write_result: Some(result.clone()),
                    error_msg: None,
                },
                lellm_agent::AgentEvent::LoopError { error, .. } => ExtractedResult {
                    write_result: None,
                    error_msg: Some(error.to_string()),
                },
                _ => ExtractedResult {
                    write_result: None,
                    error_msg: None,
                },
            };

            // 转发到 Graph 层（通过 NodeEvent 中间层）
            let _ = sink
                .send(GraphEvent::Node {
                    trace_id,
                    node_name: node_name.clone(),
                    event: NodeEvent::Agent(event),
                })
                .await;

            // 处理终态
            if let Some(result) = extracted.write_result {
                write_agent_result(self, &result, state);
                return Ok(StreamNodeResult::Done {
                    next: NextStep::GoToNext,
                    trace_id,
                });
            }
            if let Some(err_msg) = extracted.error_msg {
                return Err(GraphError::NodeExecutionFailed {
                    node: self.name.clone(),
                    source: err_msg.into(),
                });
            }
        }

        Err(GraphError::NodeExecutionFailed {
            node: self.name.clone(),
            source: "agent stream closed without terminal event".into(),
        })
    }
}

// ─── LLMNode (P3: 细粒度手动模式) ──────────────────────────────

/// 单次 LLM 调用节点。
///
/// 与 `AgentNode`（完整 ReAct 循环）不同，`LLMNode` 仅执行一次 LLM 调用，
/// 将响应写入 State。配合 `ToolNode` + `ConditionNode`，可手动构建 ReAct 循环。
///
/// ⚠️ **警告：** 使用 `LLMNode` + `ToolNode` 手动构建循环时，你将**失去**以下保护：
/// - `ParallelSafety` 并发工具执行
/// - `RetryPolicy` 自动重试
/// - `FallbackStrategy` 容错路由
/// - 输出/推理预算保险丝
/// - `Context Compaction` 上下文压缩
///
/// **适用场景（窄但真实）：**
/// 1. 自定义 Agent Loop — 实现非 ReAct 的交互模式（如 multi-agent debate）
/// 2. 调试/教学 — 逐步观察每轮 LLM 输入输出
/// 3. 混合编排 — 多个 AgentNode 之间插入自定义处理逻辑
///
/// 除非你有明确理由，否则请使用 [`AgentNode`]。
///
/// ```rust,ignore
/// // 手动 ReAct 循环：
/// GraphBuilder::new("react")
///     .start("llm")
///     .node("llm", NodeKind::Llm(LLMNode::new("llm", model)))
///     .node("tools", NodeKind::Tool(ToolNode::all(tool_executor)))
///     .node("route", NodeKind::Condition(
///         ConditionNode::builder("route")
///             .branch("tools", |s| has_tool_calls(s))
///             .branch("end", |_| true)
///             .build()
///     ))
///     .edge("llm", "route")
///     .edge("tools", "llm")
///     .end("end")
///     .build();
/// ```
pub struct LLMNode {
    pub name: String,
    model: lellm_agent::ResolvedModel,
    system_prompt: Option<String>,
    messages_key: String,
}

impl LLMNode {
    pub fn new(name: impl Into<String>, model: lellm_agent::ResolvedModel) -> Self {
        Self {
            name: name.into(),
            model,
            system_prompt: None,
            messages_key: "messages".into(),
        }
    }

    /// 设置系统提示。
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// 设置 State 中消息的 key（默认 "messages"）。
    pub fn with_messages_key(mut self, key: impl Into<String>) -> Self {
        self.messages_key = key.into();
        self
    }
}

#[async_trait]
impl GraphNode for LLMNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        // 读取消息
        let mut messages = state
            .get(&self.messages_key)
            .and_then(|v| serde_json::from_value::<Vec<lellm_core::Message>>(v.clone()).ok())
            .unwrap_or_default();

        // 注入系统提示
        if let Some(ref sys) = self.system_prompt {
            // 移除已有 system message
            messages.retain(|m| !matches!(m, lellm_core::Message::System { .. }));
            messages.insert(
                0,
                lellm_core::Message::System {
                    content: lellm_core::text_block(sys.clone()),
                },
            );
        }

        // 构建请求
        let request = lellm_core::ChatRequest {
            model: self.model.model.clone(),
            messages: messages.clone(),
            ..Default::default()
        };

        // 调用 LLM
        let response = self.model.provider.call(&request).await.map_err(|e| {
            GraphError::NodeExecutionFailed {
                node: self.name.clone(),
                source: Box::new(e),
            }
        })?;

        // 将响应追加到消息列表
        let assistant_msg = lellm_core::Message::Assistant {
            content: response.content,
        };
        messages.push(assistant_msg);
        state.insert(
            self.messages_key.clone(),
            serde_json::to_value(&messages).map_err(|e| {
                GraphError::StateError(format!("failed to serialize messages: {e}"))
            })?,
        );

        Ok(NextStep::GoToNext)
    }
}
