//! ReAct Graph — AgentFlowNode 内部构建的有环图。
//!
//! ```text
//! [LLM] --有tool_calls--> [Tool] --(自环)--> [LLM]
//!      --无tool_calls--> [End]
//! ```

use std::sync::Arc;

use async_trait::async_trait;

use lellm_core::{Message, ToolCall};
use lellm_graph::{
    FlowNode, Graph, GraphBuilder, GraphError, NodeContext, NodeKind, TerminalError,
};

use crate::runtime::config::{ToolUseConfig, ToolUseDeps};
use crate::runtime::tools::ToolExecutor;

// ─── State Keys ──────────────────────────────────────────────

/// 当前轮次工具调用 key
pub const SK_CURRENT_TOOL_CALLS: &str = "current_tool_calls";
/// 是否有工具调用 key
pub const SK_HAS_TOOL_CALLS: &str = "has_tool_calls";

// ─── LLMNode ──────────────────────────────────────────────────

/// LLM 调用节点 — 执行单次 LLM 调用。
///
/// 读取:
/// - `messages` — 消息历史
/// - `iterations` — 当前迭代轮次
///
/// 写入:
/// - `messages` — 追加 assistant 响应
/// - `current_tool_calls` — 本轮工具调用
/// - `has_tool_calls` — 是否有工具调用
/// - `iterations` — 递增
pub struct LLMNode {
    pub name: String,
    pub model: lellm_provider::ResolvedModel,
    pub executor: ToolExecutor,
    pub config: ToolUseConfig,
    pub deps: ToolUseDeps,
}

impl LLMNode {
    pub fn new(
        name: impl Into<String>,
        model: lellm_provider::ResolvedModel,
        executor: ToolExecutor,
        config: ToolUseConfig,
        deps: ToolUseDeps,
    ) -> Self {
        Self {
            name: name.into(),
            model,
            executor,
            config,
            deps,
        }
    }
}

#[async_trait]
impl FlowNode for LLMNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let messages = get_messages_from_ctx(ctx);

        // 获取工具定义
        let snapshot = self.executor.snapshot().await;
        let definitions: Vec<lellm_core::ToolDefinition> = snapshot.definitions().to_vec();

        // 构建请求
        let iterations: u32 = ctx.get("iterations").unwrap_or(0);
        let req = crate::runtime::config::build_request_inner_with_round(
            &self.model,
            &messages,
            self.config.max_output_tokens,
            &self.config.request_options,
            iterations as usize,
            &definitions,
        );

        // 执行 LLM 调用
        let response = self.model.provider.call(&req).await.map_err(|e| {
            GraphError::Terminal(TerminalError::NodeExecutionFailed {
                node: self.name.clone(),
                source: e.into(),
            })
        })?;

        // 记录输出 token
        let mut output_tokens: usize = ctx.get("output_tokens").unwrap_or(0);
        let mut reasoning_tokens: usize = ctx.get("reasoning_tokens").unwrap_or(0);
        for b in &response.content {
            match b {
                lellm_core::ContentBlock::Text(t) => {
                    output_tokens += crate::runtime::context::estimate_text(&t.text);
                }
                lellm_core::ContentBlock::Thinking(th) => {
                    reasoning_tokens += crate::runtime::context::estimate_reasoning_block(th);
                }
                _ => {}
            }
        }
        ctx.set("output_tokens", output_tokens);
        ctx.set("reasoning_tokens", reasoning_tokens);

        // 写入 assistant 响应到 messages
        let msg = Message::Assistant {
            content: response.content.clone(),
        };
        let mut messages_json: Vec<serde_json::Value> = ctx.get("messages").unwrap_or_default();
        messages_json.push(serde_json::to_value(msg).unwrap_or_default());
        ctx.set("messages", serde_json::json!(messages_json));

        // 写入工具调用
        let tool_calls: Vec<ToolCall> = response.tool_calls().cloned().collect();
        let has_tool_calls = !tool_calls.is_empty();

        ctx.set(SK_CURRENT_TOOL_CALLS, serde_json::json!(tool_calls));
        ctx.set(SK_HAS_TOOL_CALLS, has_tool_calls);

        if has_tool_calls {
            let current: usize = ctx.get("total_tool_calls").unwrap_or(0);
            ctx.set("total_tool_calls", current + 1);
        }

        // 递增迭代
        let current_iter: u32 = ctx.get("iterations").unwrap_or(0);
        ctx.set("iterations", current_iter + 1);

        tracing::debug!(
            iteration = iterations + 1,
            has_tool_calls,
            "LLM call completed"
        );

        Ok(())
    }
}

// ─── ToolNode ─────────────────────────────────────────────────

/// 工具执行节点 — 读取 tool_calls，执行工具，写入 results。
///
/// 读取:
/// - `current_tool_calls` — 本轮工具调用
///
/// 写入:
/// - `messages` — 追加工具执行结果
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
impl FlowNode for ToolNode {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let tool_calls: Vec<ToolCall> = ctx.get(SK_CURRENT_TOOL_CALLS).unwrap_or_default();

        if tool_calls.is_empty() {
            return Ok(());
        }

        let snapshot = self.executor.snapshot().await;

        // 执行工具
        let batch = crate::runtime::tools::execute_batch_with(
            &tool_calls,
            &snapshot,
            &self.executor.retry_policy(),
        )
        .await;

        if batch.panicked {
            tracing::warn!("tool batch task panicked — error results filled in by executor");
        }

        // 写入工具结果到 messages
        let budget = self.config.context_budget.clone();
        let results: Vec<Message> = batch
            .results
            .into_iter()
            .map(|m| {
                if let Message::ToolResult {
                    ref tool_call_id,
                    is_error: false,
                    ref content,
                } = m
                {
                    let truncated = budget.truncate_tool_result_blocks(content);
                    if truncated != *content {
                        return Message::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            is_error: false,
                            content: truncated,
                        };
                    }
                }
                m
            })
            .collect();

        let mut messages_json: Vec<serde_json::Value> = ctx.get("messages").unwrap_or_default();
        for msg in results {
            messages_json.push(serde_json::to_value(msg).unwrap_or_default());
        }
        ctx.set("messages", serde_json::json!(messages_json));

        tracing::debug!(tool_calls = tool_calls.len(), "tool execution completed");

        Ok(())
    }
}

// ─── ReactCondition ───────────────────────────────────────────

/// ReAct 循环条件 — 检查 tool_calls 是否为空。
///
/// 有 tool_calls → Goto("tool")
/// 无 tool_calls → End
pub struct ReactCondition {
    pub name: String,
    pub max_iterations: usize,
}

impl ReactCondition {
    pub fn new(name: impl Into<String>, max_iterations: usize) -> Self {
        Self {
            name: name.into(),
            max_iterations,
        }
    }
}

#[async_trait]
impl FlowNode for ReactCondition {
    async fn execute(&self, ctx: &mut NodeContext<'_>) -> Result<(), GraphError> {
        let has_tool_calls: bool = ctx.get(SK_HAS_TOOL_CALLS).unwrap_or(false);
        let iterations: u32 = ctx.get("iterations").unwrap_or(0);

        if has_tool_calls && (iterations as usize) < self.max_iterations {
            ctx.goto("tool");
        } else {
            ctx.end();
        }

        Ok(())
    }
}

// ─── 辅助函数 ─────────────────────────────────────────────────

fn get_messages_from_ctx(ctx: &NodeContext<'_>) -> Vec<Message> {
    ctx.get::<Vec<serde_json::Value>>("messages")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

// ─── build_react_graph ────────────────────────────────────────

/// 构建 ReAct 内部图。
///
/// ```text
/// [llm] --has_tool_calls--> [tool] --(self)--> [llm]
///      --no_tool_calls--> [end]
/// ```
pub fn build_react_graph(llm_node: LLMNode, tool_node: ToolNode, max_iterations: usize) -> Graph {
    let llm_name = llm_node.name.clone();

    let mut builder = GraphBuilder::new(format!("react_{}", llm_name));
    builder.start("llm");
    builder.end("end");

    builder.node("llm", NodeKind::External(Arc::new(llm_node)));
    builder.node("tool", NodeKind::External(Arc::new(tool_node)));
    builder.node(
        "condition",
        NodeKind::External(Arc::new(ReactCondition::new(
            format!("{}_condition", llm_name),
            max_iterations,
        ))),
    );

    // llm -> condition
    builder.edge("llm", "condition");

    // condition -> tool (有 tool_calls)
    builder.edge_if("condition", "tool", |state| {
        state
            .get("has_tool_calls")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });

    // condition -> end (无 tool_calls 或达到最大迭代)
    builder.edge_fallback("condition", "end");

    // tool -> llm (自环)
    builder.edge("tool", "llm");

    builder.build().expect("ReAct graph should be valid")
}
