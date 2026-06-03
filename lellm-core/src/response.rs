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
    /// 构造函数
    pub fn new(content: Vec<ContentBlock>, usage: TokenUsage, raw: serde_json::Value) -> Self {
        Self {
            content,
            usage,
            raw,
        }
    }

    /// 借用视图 — 零分配、零拷贝的 tool_call 迭代器
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// 是否存在 tool_calls
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls().next().is_some()
    }
}

/// Token 消耗统计。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, ToolCall};

    #[test]
    fn test_chat_response_tool_calls_iterator() {
        let tc = ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: serde_json::json!({}),
        };
        let content = vec![
            ContentBlock::text("hello".to_string()),
            ContentBlock::ToolCall(tc.clone()),
        ];
        let resp = ChatResponse::new(content, TokenUsage::default(), serde_json::json!(null));
        let calls: Vec<_> = resp.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "1");
    }

    #[test]
    fn test_chat_response_has_tool_calls() {
        let tc = ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: serde_json::json!({}),
        };
        let content = vec![
            ContentBlock::text("hello".to_string()),
            ContentBlock::ToolCall(tc),
        ];
        let resp = ChatResponse::new(content, TokenUsage::default(), serde_json::json!(null));
        assert!(resp.has_tool_calls());
    }

    #[test]
    fn test_chat_response_no_tool_calls() {
        let content = vec![ContentBlock::text("hello".to_string())];
        let resp = ChatResponse::new(content, TokenUsage::default(), serde_json::json!(null));
        assert!(!resp.has_tool_calls());
    }
}
