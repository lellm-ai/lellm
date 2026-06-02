//! 响应类型。

use serde::{Deserialize, Serialize};

use super::{ContentBlock, ToolCall};

/// 统一的聊天响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 响应内容块列表，与 `Message::Assistant` 的 content 类型对齐。
    pub content: Vec<ContentBlock>,
    /// 冗余缓存，从 content 中提取的 tool_calls，方便访问。
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub raw: serde_json::Value,
}

impl ChatResponse {
    /// 构造函数 — 自动从 content 中提取 tool_calls
    pub fn new(content: Vec<ContentBlock>, usage: TokenUsage, raw: serde_json::Value) -> Self {
        let tool_calls = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
        Self {
            content,
            tool_calls,
            usage,
            raw,
        }
    }
}

/// Token 消耗统计。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
