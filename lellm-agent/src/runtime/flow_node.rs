//! AgentFlowNode — 将 ToolUseLoop 包装为 Graph FlowNode。
//!
//! 在 Graph 编排中作为节点执行 Agent Loop，读写 State 中的消息。

use async_trait::async_trait;

use lellm_graph::node::StreamNodeResult;
use lellm_graph::{FlowEvent, FlowNode, GraphError, NextStep, NodeOutput};
use lellm_graph::{GraphEvent, SpanId, State, StateDelta, TerminalError};

use crate::hook::{AgentHook, AgentHookContext, AgentHookSnapshot};
use crate::runtime::{AgentEvent, ToolUseLoop, ToolUseResult};

/// Agent 在 Graph 中的节点包装。
///
/// 将 `ToolUseLoop` 适配为 `FlowNode`，使其可以作为 Graph 的节点执行。
///
/// # State 约定
///
/// - 输入: `state.get("messages")` → `Vec<serde_json::Value>` 或 `serde_json::Value` 数组
/// - 输出: `state.set("messages")` → 更新后的消息列表
/// - 自定义 key: 通过 `message_key` 配置
///
/// # 示例
///
/// ```rust,ignore
/// use lellm_agent::AgentFlowNode;
/// use lellm_graph::{GraphBuilder, NodeKind};
///
/// let agent = AgentFlowNode::new("agent", tool_use_loop);
/// let mut graph = GraphBuilder::new("my_graph");
/// graph.node("agent", NodeKind::External(Arc::new(agent)));
/// ```
#[derive(Clone)]
pub struct AgentFlowNode {
    /// 节点名称
    name: String,
    /// Agent 执行循环
    loop_: ToolUseLoop,
    /// State 中消息的 key（默认 "messages"）
    message_key: String,
    /// 是否使用流式模式（仅在 execute_stream 中生效）
    stream_events: bool,
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
            stream_events: true,
            hooks: Vec::new(),
        }
    }

    /// 设置 State 中消息的 key（默认 "messages"）。
    pub fn message_key(mut self, key: impl Into<String>) -> Self {
        self.message_key = key.into();
        self
    }

    /// 是否发射流式事件到 sink（默认 true）。
    pub fn stream_events(mut self, enabled: bool) -> Self {
        self.stream_events = enabled;
        self
    }

    /// 添加 Agent-level hook。
    ///
    /// Hook 在 agent loop 执行前后调用，返回的 Deltas 会合并到 State。
    pub fn hook(mut self, hook: impl AgentHook + 'static) -> Self {
        self.hooks.push(std::sync::Arc::new(hook));
        self
    }

    /// 从 State 中提取输入消息。
    fn extract_messages(&self, state: &State) -> Vec<lellm_core::Message> {
        // 尝试从 State 中读取 messages
        // 格式: Vec<serde_json::Value>，每个 Value 是一个 Message 的 JSON 表示
        if let Some(value) = state.get(&self.message_key) {
            // 如果是数组，逐个解析
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

    /// 将执行结果转换为 StateDelta 列表。
    fn collect_deltas(&self, result: &ToolUseResult) -> Vec<StateDelta> {
        // 将最终消息列表序列化为 JSON
        let messages: Vec<serde_json::Value> = result
            .messages
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();

        vec![
            StateDelta::put(&self.message_key, serde_json::json!(messages)),
            StateDelta::put(
                format!("{}_stop_reason", self.name),
                serde_json::json!(format!("{:?}", result.stop_reason)),
            ),
            StateDelta::put(
                format!("{}_iterations", self.name),
                serde_json::json!(result.iterations),
            ),
            StateDelta::put(
                format!("{}_tool_calls", self.name),
                serde_json::json!(result.tool_calls_executed),
            ),
        ]
    }
}

#[async_trait]
impl FlowNode for AgentFlowNode {
    /// 阻塞执行 — 运行完整的 Agent Loop。
    async fn execute(&self, state: &State) -> Result<NodeOutput, GraphError> {
        let messages = self.extract_messages(state);

        // 如果没有消息，发送一个警告但仍继续执行（agent 可能只需要 system prompt）
        if messages.is_empty() {
            tracing::debug!(
                agent = %self.name,
                "no input messages found in state key '{}'",
                self.message_key
            );
        }

        // 调用 before_agent hooks
        let ctx = AgentHookContext {
            node_name: self.name.clone(),
            input_message_count: messages.len(),
        };
        let mut hook_deltas: Vec<StateDelta> = self
            .hooks
            .iter()
            .flat_map(|h| h.before_agent(&ctx))
            .collect();

        let result = self.loop_.execute(messages).await.map_err(|e| {
            GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: self.name.clone(),
                source: e.into(),
            })
        })?;

        // 调用 after_agent hooks
        let snapshot = AgentHookSnapshot {
            result: result.clone(),
            events: Vec::new(), // 非流式模式无事件
        };
        hook_deltas.extend(self.hooks.iter().flat_map(|h| h.after_agent(&snapshot)));

        let mut deltas = self.collect_deltas(&result);
        deltas.extend(hook_deltas);

        tracing::debug!(
            agent = %self.name,
            iterations = result.iterations,
            tool_calls = result.tool_calls_executed,
            stop_reason = ?result.stop_reason,
            "agent execution completed"
        );

        Ok(NodeOutput {
            deltas,
            next: NextStep::GoToNext,
        })
    }

    /// 流式执行 — 运行 Agent Loop 并转发事件。
    async fn execute_stream(
        &self,
        state: &State,
        sink: &tokio::sync::mpsc::Sender<GraphEvent>,
        span_id: SpanId,
    ) -> Result<StreamNodeResult, GraphError> {
        let messages = self.extract_messages(state);

        // 调用 before_agent hooks
        let ctx = AgentHookContext {
            node_name: self.name.clone(),
            input_message_count: messages.len(),
        };
        let hook_deltas: Vec<StateDelta> = self
            .hooks
            .iter()
            .flat_map(|h| h.before_agent(&ctx))
            .collect();

        // 发射 NodeStarted 事件
        let _ = sink
            .send(GraphEvent::Node {
                span_id,
                node_name: self.name.clone(),
                event: FlowEvent::NodeStarted {
                    node_id: self.name.clone(),
                    span_id,
                },
            })
            .await;

        // 启动流式 Agent Loop
        let mut agent_stream = self.loop_.execute_stream(messages);

        let mut final_result: Option<ToolUseResult> = None;
        let mut error_delta: Option<StateDelta> = None;
        let mut events: Vec<AgentEvent> = Vec::new();

        while let Some(agent_event) = agent_stream.recv().await {
            // 检查是否是终态事件
            let is_terminal = matches!(
                &agent_event,
                AgentEvent::LoopEnd { .. } | AgentEvent::LoopError { .. }
            );

            // 收集事件（用于 after_agent hook）
            events.push(agent_event.clone());

            // 转发事件（如果启用）
            if self.stream_events {
                let _ = sink
                    .send(GraphEvent::Node {
                        span_id,
                        node_name: self.name.clone(),
                        event: FlowEvent::Custom {
                            node_id: self.name.clone(),
                            payload: Box::new(agent_event.clone()),
                        },
                    })
                    .await;
            }

            // 处理终态事件
            if is_terminal {
                match &agent_event {
                    AgentEvent::LoopEnd { result } => {
                        final_result = Some(result.clone());
                    }
                    AgentEvent::LoopError { error, .. } => {
                        // 错误信息转为 Delta
                        error_delta = Some(StateDelta::put(
                            format!("{}_error", self.name),
                            serde_json::json!(error.to_string()),
                        ));
                    }
                    _ => {}
                }
            }
        }

        // 如果有错误，返回 Fallback 让 Graph 决定如何处理
        if let Some(err_delta) = error_delta {
            return Ok(StreamNodeResult::Fallback {
                deltas: hook_deltas
                    .into_iter()
                    .chain(std::iter::once(err_delta))
                    .collect(),
                reason: format!("agent loop error in '{}'", self.name),
                node_name: self.name.clone(),
            });
        }

        // 写入最终结果
        if let Some(result) = final_result {
            // 调用 after_agent hooks
            let snapshot = AgentHookSnapshot {
                result: result.clone(),
                events,
            };
            let after_deltas: Vec<StateDelta> = self
                .hooks
                .iter()
                .flat_map(|h| h.after_agent(&snapshot))
                .collect();

            let mut deltas = self.collect_deltas(&result);
            deltas.extend(hook_deltas);
            deltas.extend(after_deltas);

            // 发射 NodeCompleted 事件
            let _ = sink
                .send(GraphEvent::Node {
                    span_id,
                    node_name: self.name.clone(),
                    event: FlowEvent::NodeCompleted {
                        node_id: self.name.clone(),
                        span_id,
                        duration: std::time::Duration::ZERO, // 由 executor 计算
                    },
                })
                .await;

            return Ok(StreamNodeResult::Continue {
                deltas,
                next: NextStep::GoToNext,
                span_id,
                observed: None,
            });
        }

        // 没有收到终态事件（channel 意外关闭）
        Ok(StreamNodeResult::Fallback {
            deltas: hook_deltas,
            reason: "agent stream ended without terminal event".into(),
            node_name: self.name.clone(),
        })
    }
}
