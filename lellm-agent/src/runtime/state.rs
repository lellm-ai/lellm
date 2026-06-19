//! Agent 状态辅助 — 基于 Graph State 的统一状态访问。
//!
//! 替代原来的 LoopState，所有 Agent 状态统一存储在 Graph State 中，
//! 实现单一事实来源（SSOT）。
//!
//! # 状态键命名约定
//!
//! - 消息：用户自选 key（默认 `"messages"`）
//! - 其他：`"{agent_name}/{suffix}"` 命名空间隔离

use std::collections::HashMap;

use lellm_core::{ChatResponse, Message};
use serde_json::Value;

use super::ToolUseResult;
use super::context::{estimate_reasoning_block, estimate_text, estimate_tokens};
use super::event::StopReason;

// ─── 状态键后缀（与 agent_name 组合使用）─────────────────────────

/// 迭代计数后缀
pub const SK_ITERATIONS: &str = "iterations";
/// 已执行工具调用数后缀
pub const SK_TOOL_CALLS: &str = "tool_calls";
/// 停止原因后缀
pub const SK_STOP_REASON: &str = "stop_reason";
/// 累计输出 Token 后缀
pub const SK_OUTPUT_TOKENS: &str = "output_tokens";
/// 累计推理 Token 后缀
pub const SK_REASONING_TOKENS: &str = "reasoning_tokens";

// ─── 状态键构建工具 ──────────────────────────────────────────────

/// 构建 Agent 状态键（带命名空间）。
pub fn agent_key(name: &str, suffix: &str) -> String {
    format!("{name}/{suffix}")
}

/// 从 State 中读取 usize 值。
pub fn get_usize(state: &HashMap<String, Value>, key: &str) -> usize {
    state
        .get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or_default()
}

/// 向 State 中写入 usize 值。
pub fn set_usize(state: &mut HashMap<String, Value>, key: &str, value: usize) {
    state.insert(key.to_string(), Value::from(value as u64));
}

/// 从 State 中读取消息列表。
pub fn get_messages(state: &HashMap<String, Value>, key: &str) -> Vec<Message> {
    if let Some(value) = state.get(key) {
        if let Some(arr) = value.as_array() {
            let mut messages = Vec::new();
            for v in arr {
                if let Ok(msg) = serde_json::from_value::<Message>(v.clone()) {
                    messages.push(msg);
                }
            }
            return messages;
        }
        // 单个消息
        if let Ok(msg) = serde_json::from_value::<Message>(value.clone()) {
            return vec![msg];
        }
    }
    Vec::new()
}

/// 将消息列表写入 State。
pub fn set_messages(state: &mut HashMap<String, Value>, key: &str, messages: &[Message]) {
    let json: Vec<Value> = messages
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    state.insert(key.to_string(), Value::Array(json));
}

/// 从 State 中读取 StopReason。
pub fn get_stop_reason(state: &HashMap<String, Value>, key: &str) -> Option<StopReason> {
    state
        .get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "Complete" => Some(StopReason::Complete),
            "MaxIterationsReached" => Some(StopReason::MaxIterationsReached),
            "Cancelled" => Some(StopReason::Cancelled),
            "OutputBudgetExceeded" => Some(StopReason::OutputBudgetExceeded),
            "ReasoningBudgetExceeded" => Some(StopReason::ReasoningBudgetExceeded),
            _ => None,
        })
}

/// 将 StopReason 写入 State。
pub fn set_stop_reason(state: &mut HashMap<String, Value>, key: &str, reason: &StopReason) {
    state.insert(key.to_string(), Value::String(format!("{:?}", reason)));
}

// ─── AgentState — 统一状态访问器 ─────────────────────────────────

/// Agent 执行期间的统一状态访问器。
///
/// 替代 LoopState，所有读写直接操作 Graph State。
/// 提供类型安全的方法访问 Agent 相关状态。
pub struct AgentState<'a> {
    /// Graph State 引用
    state: &'a mut HashMap<String, Value>,
    /// Agent 名称（用于键命名空间）
    agent_name: String,
    /// 消息的 State key
    message_key: String,
}

impl<'a> AgentState<'a> {
    pub fn new(state: &'a mut HashMap<String, Value>, agent_name: &str, message_key: &str) -> Self {
        Self {
            state,
            agent_name: agent_name.to_string(),
            message_key: message_key.to_string(),
        }
    }

    // ─── 键访问器 ─────────────────────────────────────────────

    fn key(&self, suffix: &str) -> String {
        agent_key(&self.agent_name, suffix)
    }

    pub fn message_key(&self) -> &str {
        &self.message_key
    }

    // ─── 消息 ─────────────────────────────────────────────────

    /// 读取当前消息列表。
    pub fn messages(&self) -> Vec<Message> {
        get_messages(self.state, &self.message_key)
    }

    /// 设置消息列表。
    pub fn set_messages(&mut self, messages: Vec<Message>) {
        set_messages(self.state, &self.message_key, &messages);
    }

    /// 追加 Assistant 消息。
    pub fn push_assistant(&mut self, content: Vec<lellm_core::ContentBlock>) {
        let msg = Message::Assistant {
            content: content.clone(),
        };
        let tokens = estimate_tokens(&[msg.clone()]);
        // 更新 estimated_tokens（如果有的话）
        let est_key = self.key("estimated_tokens");
        let current: usize = get_usize(self.state, &est_key);
        set_usize(self.state, &est_key, current + tokens);

        let mut msgs = self.messages();
        msgs.push(msg);
        set_messages(self.state, &self.message_key, &msgs);
    }

    /// 追加工具结果消息。
    pub fn push_tool_results(&mut self, results: Vec<Message>) {
        let tokens = estimate_tokens(&results);
        let est_key = self.key("estimated_tokens");
        let current: usize = get_usize(self.state, &est_key);
        set_usize(self.state, &est_key, current + tokens);

        let mut msgs = self.messages();
        msgs.extend(results);
        set_messages(self.state, &self.message_key, &msgs);
    }

    // ─── 迭代 ─────────────────────────────────────────────────

    /// 进入下一轮迭代。
    pub fn next_iteration(&mut self) {
        let key = self.key(SK_ITERATIONS);
        let current: usize = get_usize(self.state, &key);
        set_usize(self.state, &key, current + 1);
    }

    /// 当前迭代数。
    pub fn iterations(&self) -> usize {
        get_usize(self.state, &self.key(SK_ITERATIONS))
    }

    /// 是否达到最大迭代数。
    pub fn reached_max(&self, max: usize) -> bool {
        self.iterations() >= max
    }

    // ─── 工具调用 ─────────────────────────────────────────────

    /// 记录工具调用数量。
    pub fn add_tool_calls(&mut self, count: usize) {
        let key = self.key(SK_TOOL_CALLS);
        let current: usize = get_usize(self.state, &key);
        set_usize(self.state, &key, current + count);
    }

    /// 已执行工具调用数。
    pub fn tool_calls_executed(&self) -> usize {
        get_usize(self.state, &self.key(SK_TOOL_CALLS))
    }

    // ─── Token 统计 ───────────────────────────────────────────

    /// 累计输出 Token。
    pub fn add_output_tokens(&mut self, tokens: usize) {
        let key = self.key(SK_OUTPUT_TOKENS);
        let current: usize = get_usize(self.state, &key);
        set_usize(self.state, &key, current + tokens);
    }

    /// 累计推理 Token。
    pub fn add_reasoning_tokens(&mut self, tokens: usize) {
        let key = self.key(SK_REASONING_TOKENS);
        let current: usize = get_usize(self.state, &key);
        set_usize(self.state, &key, current + tokens);
    }

    /// 当前输出 Token 总数。
    pub fn total_output_tokens(&self) -> usize {
        get_usize(self.state, &self.key(SK_OUTPUT_TOKENS))
    }

    /// 当前推理 Token 总数。
    pub fn total_reasoning_tokens(&self) -> usize {
        get_usize(self.state, &self.key(SK_REASONING_TOKENS))
    }

    /// 从 ContentBlock 分离估算 Output 和 Reasoning Token。
    pub fn add_output_from_content(&mut self, content: &[lellm_core::ContentBlock]) {
        let mut output_tokens: usize = 0;
        let mut reasoning_tokens: usize = 0;
        for b in content {
            match b {
                lellm_core::ContentBlock::Text(t) => output_tokens += estimate_text(&t.text),
                lellm_core::ContentBlock::Thinking(th) => {
                    reasoning_tokens += estimate_reasoning_block(th)
                }
                lellm_core::ContentBlock::Image { .. } | lellm_core::ContentBlock::ToolCall(_) => {}
            }
        }
        self.add_output_tokens(output_tokens);
        self.add_reasoning_tokens(reasoning_tokens);
    }

    /// 检查是否超过输出预算。
    pub fn exceeded_total_output(&self, max: Option<u32>) -> bool {
        match max {
            Some(limit) => self.total_output_tokens() >= limit as usize,
            None => false,
        }
    }

    /// 检查是否超过推理预算。
    pub fn exceeded_total_reasoning(&self, max: Option<u32>) -> bool {
        match max {
            Some(limit) => self.total_reasoning_tokens() >= limit as usize,
            None => false,
        }
    }

    // ─── 初始化 ───────────────────────────────────────────────

    /// 初始化消息和 estimated_tokens。
    pub fn init_messages(&mut self, messages: Vec<Message>) {
        let tokens = estimate_tokens(&messages);
        set_messages(self.state, &self.message_key, &messages);
        set_usize(self.state, &self.key("estimated_tokens"), tokens);
        set_usize(self.state, &self.key(SK_ITERATIONS), 0);
        set_usize(self.state, &self.key(SK_TOOL_CALLS), 0);
        set_usize(self.state, &self.key(SK_OUTPUT_TOKENS), 0);
        set_usize(self.state, &self.key(SK_REASONING_TOKENS), 0);
    }

    // ─── 构建结果 ─────────────────────────────────────────────

    /// 构建 ToolUseResult。
    pub fn finish(&self, stop_reason: StopReason, response: ChatResponse) -> ToolUseResult {
        ToolUseResult {
            stop_reason,
            response,
            messages: self.messages(),
            iterations: self.iterations(),
            tool_calls_executed: self.tool_calls_executed(),
        }
    }

    pub fn finish_complete(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::Complete, response)
    }

    pub fn finish_max_iterations(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::MaxIterationsReached, response)
    }

    pub fn finish_cancelled(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::Cancelled, response)
    }

    pub fn finish_output_budget(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::OutputBudgetExceeded, response)
    }

    pub fn finish_reasoning_budget(&self, response: ChatResponse) -> ToolUseResult {
        self.finish(StopReason::ReasoningBudgetExceeded, response)
    }

    /// 写入停止原因到 State。
    pub fn set_stop_reason(&mut self, reason: &StopReason) {
        set_stop_reason(self.state, &self.key(SK_STOP_REASON), reason);
    }

    /// 获取底层的 State 引用（用于高级操作）。
    pub fn inner(&self) -> &HashMap<String, Value> {
        self.state
    }

    /// 获取底层的可变 State 引用。
    pub fn inner_mut(&mut self) -> &mut HashMap<String, Value> {
        self.state
    }
}

// ─── 从 State 构建 ToolUseResult（不依赖 AgentState）────────────

/// 从 State 中读取数据构建 ToolUseResult（用于流式模式的终端处理）。
pub fn build_result_from_state(
    state: &HashMap<String, Value>,
    agent_name: &str,
    message_key: &str,
    stop_reason: StopReason,
    response: ChatResponse,
) -> ToolUseResult {
    let messages = get_messages(state, message_key);
    let iterations = get_usize(state, &agent_key(agent_name, SK_ITERATIONS));
    let tool_calls_executed = get_usize(state, &agent_key(agent_name, SK_TOOL_CALLS));

    ToolUseResult {
        stop_reason,
        response,
        messages,
        iterations,
        tool_calls_executed,
    }
}

/// 便捷创建初始 State — 将消息列表放入 State。
///
/// # 示例
///
/// ```rust,ignore
/// use lellm_agent::{ToolUseLoop, runtime::state::initial_state};
/// use lellm_core::Message;
///
/// let state = initial_state(vec![Message::user("hello")]);
/// let result = agent.execute(&mut state, "messages").await?;
/// ```
pub fn initial_state(messages: Vec<Message>, message_key: &str) -> lellm_graph::State {
    let mut state = lellm_graph::State::new();
    set_messages(&mut state, message_key, &messages);
    set_usize(
        &mut state,
        &agent_key("agent", "estimated_tokens"),
        estimate_tokens(&messages),
    );
    set_usize(&mut state, &agent_key("agent", SK_ITERATIONS), 0);
    set_usize(&mut state, &agent_key("agent", SK_TOOL_CALLS), 0);
    set_usize(&mut state, &agent_key("agent", SK_OUTPUT_TOKENS), 0);
    set_usize(&mut state, &agent_key("agent", SK_REASONING_TOKENS), 0);
    state
}
