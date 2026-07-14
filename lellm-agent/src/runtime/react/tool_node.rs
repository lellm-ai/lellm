//! 工具执行节点 — 读取 tool_calls，执行工具，写入 results。
//!
//! # Typed State
//!
//! 从 ctx 获取 `AgentState`，执行工具，追加结果到消息历史。
//!
//! **执行路径：**
//! ```text
//! ToolNode::execute()
//!   └── executor.execute_batch()   ← ParallelSafety 调度
//!       └── dispatch_one()         ← lookup + retry + invoke
//!           └── ExecutableTool::execute()
//!   └── 后处理（budget truncate, cache marker, event emit）
//! ```

use async_trait::async_trait;

use lellm_core::{CacheControl, TextBlock, ToolCall};
use lellm_graph::{GraphError, LeafContext, LeafNode};

use super::super::config::ToolUseConfig;
use super::super::context::ContextBudget;
use super::super::tools::ToolExecutor;
use super::super::context_ext::AgentContextExt;
use super::super::typed_state::AgentState;

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

        // 1. 获取工具调用（纯引用，零 clone）
        let tool_calls: Vec<ToolCall> = ctx
            .state()
            .last_response
            .as_ref()
            .map(|resp| resp.tool_calls().cloned().collect())
            .unwrap_or_default();

        if tool_calls.is_empty() {
            return Ok(());
        }

        // 2. Emit Queued + Started for all tools（严格按 ToolCall 顺序）
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

        // 3. 统一执行入口 — ParallelSafety 分组调度
        let batch = self.executor.execute_batch(&tool_calls).await;

        if batch.panicked {
            tracing::warn!("tool batch task panicked — error results filled");
        }

        // 4. 后处理 + event emit + 收集结果（单次遍历）
        let budget = self.config.context_budget.clone();
        let mut processed_messages = Vec::with_capacity(tool_calls.len());

        for (call, msg, duration) in tool_calls
            .iter()
            .zip(batch.results.into_iter())
            .zip(batch.durations.into_iter())
            .map(|((call, msg), duration)| (call, msg, duration))
        {
            let processed = apply_post_process(msg, &budget);

            // Emit Finished
            ctx.emit(StreamChunk::ToolLifecycle {
                phase: ToolPhase::Finished,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
            });

            // Emit ToolOutput
            if let Some(chunk) = tool_output_chunk(&processed, &call.id, &call.name, duration) {
                ctx.emit(chunk);
            }

            processed_messages.push(processed);
        }

        // 5. 追加 Tool 结果消息
        ctx.append_messages(processed_messages);

        tracing::debug!(tool_calls = tool_calls.len(), "tool execution completed");

        Ok(())
    }
}

/// 后处理：budget truncate + cache marker
fn apply_post_process(msg: lellm_core::Message, budget: &ContextBudget) -> lellm_core::Message {
    let msg = apply_budget_truncate(msg, budget);
    set_tool_result_cache(msg)
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
