//! 响应类型。

use serde::{Deserialize, Serialize};

use super::{ContentBlock, ToolCall};

/// 统一的聊天响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 响应内容块列表，与 `Message::Assistant` 的 content 类型对齐。
    pub content: Vec<ContentBlock>,
    pub usage: TokenUsage,
    pub raw: serde_json::Value,
}

impl ChatResponse {
    /// 提取所有文本块拼接为字符串（便捷方法）。
    pub fn extract_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join("")
    }

    /// 提取所有 ToolCall。
    pub fn extract_tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::ToolCall(tc) = b {
                    Some(tc.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall(_)))
    }
}

/// Token 消耗统计。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
