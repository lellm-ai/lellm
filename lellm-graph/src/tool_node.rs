//! 工具执行节点。

use async_trait::async_trait;

use crate::error::{GraphError, TerminalError};
use crate::node::GraphNode;
use crate::node::NextStep;
use crate::state::State;

/// 工具执行节点。
///
/// 读取 State 中最后一条 Assistant 消息的 `tool_calls`，
/// 执行所有工具调用，将 `ToolResult` 消息追加到消息列表。
///
/// ⚠️ **警告：** 此节点是 `LLMNode` 的配套组件，用于手动构建 ReAct 循环。
/// 与 [`AgentNode`](crate::AgentNode) 不同，**不提供** `ParallelSafety` 并发执行、
/// `RetryPolicy` 自动重试、`FallbackStrategy` 容错等保护。
///
/// 除非你有明确理由需要手动控制每轮 LLM 调用，否则请使用 [`AgentNode`](crate::AgentNode)。
pub struct ToolNode {
    pub name: String,
    executor: lellm_agent::ToolExecutor,
    messages_key: String,
}

impl ToolNode {
    /// 创建包含所有注册工具的 ToolNode。
    pub fn all(executor: lellm_agent::ToolExecutor) -> Self {
        Self {
            name: "tools".into(),
            executor,
            messages_key: "messages".into(),
        }
    }

    /// 创建指定名称的 ToolNode。
    pub fn new(name: impl Into<String>, executor: lellm_agent::ToolExecutor) -> Self {
        Self {
            name: name.into(),
            executor,
            messages_key: "messages".into(),
        }
    }

    /// 设置 State 中消息的 key（默认 "messages"）。
    pub fn with_messages_key(mut self, key: impl Into<String>) -> Self {
        self.messages_key = key.into();
        self
    }
}

#[async_trait]
impl GraphNode for ToolNode {
    async fn execute(&self, state: &mut State) -> Result<NextStep, GraphError> {
        let messages = state
            .get(&self.messages_key)
            .and_then(|v| serde_json::from_value::<Vec<lellm_core::Message>>(v.clone()).ok())
            .unwrap_or_default();

        if messages.is_empty() {
            return Ok(NextStep::GoToNext);
        }

        // 获取最后一条消息的 tool_calls
        let last_msg = messages.last().ok_or(GraphError::Terminal(TerminalError::StateError(
            "no messages to extract tool_calls from".into(),
        )))?;

        let tool_calls = match last_msg {
            lellm_core::Message::Assistant { content } => content
                .iter()
                .filter_map(|b| match b {
                    lellm_core::ContentBlock::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };

        if tool_calls.is_empty() {
            return Ok(NextStep::GoToNext);
        }

        // 执行所有工具调用
        let mut result_messages = messages;
        let snapshot = self.executor.snapshot().await;

        for tc in &tool_calls {
            let tool_result: lellm_agent::ToolResult =
                self.executor.execute_with_snapshot(tc, &snapshot).await;

            let tool_result_msg = lellm_core::Message::ToolResult {
                tool_call_id: tc.id.clone(),
                is_error: tool_result.is_err(),
                content: lellm_core::text_block(match &tool_result {
                    Ok(v) => v.to_string(),
                    Err(e) => e.to_string(),
                }),
            };
            result_messages.push(tool_result_msg);
        }

        state.insert(
            self.messages_key.clone(),
            serde_json::to_value(&result_messages).map_err(|e| {
                GraphError::Terminal(TerminalError::StateError(format!("failed to serialize messages: {e}")))
            })?,
        );

        Ok(NextStep::GoToNext)
    }
}
