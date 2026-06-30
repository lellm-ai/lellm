//! 工具执行节点 — 读取 tool_calls，执行工具，写入 results。
//!
//! # Typed State
//!
//! 从 ctx 获取 `AgentState`，执行工具，追加结果到消息历史。

use async_trait::async_trait;

use lellm_core::{CacheControl, TextBlock, ToolCall};
use lellm_graph::{GraphError, LeafContext, LeafNode};

use super::super::config::{ToolUseConfig, empty_response};
use super::super::context::ContextBudget;
use super::super::runtime::ResolvedRound;
use super::super::tools::ToolExecutor;
use super::super::typed_state::{AgentMutation, AgentState};

/// 工具执行节点。
#[derive(Clone)]
pub struct ToolNode {
    pub name: String,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
}

impl ToolNode {
    pub fn new(name: impl Into<String>, executor: ToolExecutor, config: ToolUseConfig) -> Self {
        Self {
            name: name.into(),
            executor,
            config,
        }
    }
}

#[async_trait]
impl LeafNode<AgentState> for ToolNode {
    async fn execute(&self, ctx: &mut LeafContext<'_, AgentState>) -> Result<(), GraphError> {
        use lellm_graph::{StreamChunk, ToolPhase};

        // 1. 获取工具调用
        let round = ResolvedRound::new(self.executor.snapshot().await);
        let state = ctx.state().clone();
        let last_response = state.last_response.unwrap_or_else(empty_response);
        let tool_calls: Vec<ToolCall> = last_response.tool_calls().cloned().collect();

        if tool_calls.is_empty() {
            return Ok(());
        }

        // 2. Emit Queued + Started for all tools (严格按 ToolCall 顺序)
        for call in &tool_calls {
            ctx.emit(StreamChunk::ToolLifecycle {
                phase: ToolPhase::Queued,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
            });
            ctx.emit(StreamChunk::ToolLifecycle {
                phase: ToolPhase::Started,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
            });
        }

        // 3. 并发执行每个工具，完成后立即 emit Finished + ToolOutput
        let retry_policy = self.executor.retry_policy().clone();
        let snapshot = round.snapshot.clone();
        let budget = self.config.context_budget.clone();

        let mut handles = Vec::with_capacity(tool_calls.len());
        for call in &tool_calls {
            let entry = snapshot.get(&call.name).cloned();
            let rp = retry_policy.clone();
            let call_clone = call.clone();
            let budget_clone = budget.clone();

            handles.push(tokio::spawn(async move {
                let start = std::time::Instant::now();
                let result: lellm_core::ToolResult = match entry {
                    Some(reg) => {
                        rp.execute_with_retry(&reg.func, &call_clone.arguments)
                            .await
                    }
                    None => Err(lellm_core::ToolError::not_found(format!(
                        "unknown tool: {}",
                        call_clone.name
                    ))),
                };
                let duration = start.elapsed();

                // 应用预算截断 + 前缀缓存 Breakpoint
                let msg = lellm_core::Message::tool_result(&call_clone, &result);
                let msg = apply_budget_truncate(msg, &budget_clone);
                let msg = set_tool_result_cache(msg);

                (msg, duration)
            }));
        }

        // 4. 收集结果，join_all 保持顺序，i-th result 对应 i-th tool_call
        let mut results: Vec<Option<lellm_core::Message>> = vec![None; tool_calls.len()];
        let mut panicked = false;

        let collect = futures_util::future::join_all(handles).await;
        for (i, (call, join_result)) in tool_calls.iter().zip(collect).enumerate() {
            match join_result {
                Ok((msg, duration)) => {
                    // Emit Finished
                    ctx.emit(StreamChunk::ToolLifecycle {
                        phase: ToolPhase::Finished,
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                    });
                    // Emit ToolOutput
                    if let Some(chunk) = tool_output_chunk(&msg, &call.id, &call.name, duration) {
                        ctx.emit(chunk);
                    }
                    results[i] = Some(msg);
                }
                Err(join_err) => {
                    panicked = true;
                    let err_msg = lellm_core::Message::tool_result(
                        call,
                        &Err(lellm_core::ToolError {
                            kind: lellm_core::ToolErrorKind::Internal,
                            message: format!("tool task panicked: {join_err}"),
                        }),
                    );
                    let err_msg = set_tool_result_cache(err_msg);
                    ctx.emit(StreamChunk::ToolLifecycle {
                        phase: ToolPhase::Finished,
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                    });
                    if let Some(chunk) =
                        tool_output_chunk(&err_msg, &call.id, &call.name, std::time::Duration::ZERO)
                    {
                        ctx.emit(chunk);
                    }
                    results[i] = Some(err_msg);
                }
            }
        }

        if panicked {
            tracing::warn!("tool batch task panicked — error results filled");
        }

        // 5. Emit 消息追加 Mutation
        ctx.record(AgentMutation::AppendMessages(
            results.into_iter().flatten().collect(),
        ));

        tracing::debug!(tool_calls = tool_calls.len(), "tool execution completed");

        Ok(())
    }
}

/// 对单个 ToolResult 应用预算截断。
fn apply_budget_truncate(msg: lellm_core::Message, budget: &ContextBudget) -> lellm_core::Message {
    if let lellm_core::Message::ToolResult {
        ref tool_call_id,
        is_error: false,
        ref content,
    } = msg
    {
        let truncated = budget.truncate_tool_result_blocks(content);
        if truncated != *content {
            return lellm_core::Message::ToolResult {
                tool_call_id: tool_call_id.clone(),
                is_error: false,
                content: truncated,
            };
        }
    }
    msg
}

/// 为 ToolResult 的 TextBlock 添加 cache_control Breakpoint。
///
/// ToolResult 在 Anthropic 协议中等价于 role="user" 消息，成为对话历史的一部分。
/// 添加 Breakpoint 后，后续轮次可命中前缀缓存（包含之前的工具执行结果）。
fn set_tool_result_cache(msg: lellm_core::Message) -> lellm_core::Message {
    if let lellm_core::Message::ToolResult {
        tool_call_id,
        is_error,
        content,
    } = msg
    {
        let new_content = content
            .into_iter()
            .map(|block| match block {
                lellm_core::ContentBlock::Text(tb) => lellm_core::ContentBlock::Text(TextBlock {
                    text: tb.text,
                    cache_control: Some(CacheControl::Breakpoint),
                }),
                other => other,
            })
            .collect();
        lellm_core::Message::ToolResult {
            tool_call_id,
            is_error,
            content: new_content,
        }
    } else {
        msg
    }
}

/// 从 Message::ToolResult 提取内容，构建 ToolOutput chunk。
fn tool_output_chunk(
    msg: &lellm_core::Message,
    call_id: &str,
    tool_name: &str,
    duration: std::time::Duration,
) -> Option<lellm_graph::StreamChunk> {
    if let lellm_core::Message::ToolResult {
        content, is_error, ..
    } = msg
    {
        let content_str: String = content
            .iter()
            .filter_map(|b| match b {
                lellm_core::ContentBlock::Text(t) => Some(t.text.clone()),
                lellm_core::ContentBlock::Image { .. }
                | lellm_core::ContentBlock::Thinking(_)
                | lellm_core::ContentBlock::ToolCall(_) => None,
            })
            .collect::<Vec<_>>()
            .join("");
        Some(lellm_graph::StreamChunk::ToolOutput {
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            content: content_str,
            is_error: *is_error,
            duration,
        })
    } else {
        None
    }
}
