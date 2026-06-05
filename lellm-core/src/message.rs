//! 消息与内容块类型。

use serde::{Deserialize, Serialize};

/// 纯文本块
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    pub text: String,
}

/// 思考块（Claude thinking / OpenAI reasoning）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThinkingBlock {
    pub thinking: String,
    /// 部分 provider 支持 redacted thinking
    pub redacted: Option<String>,
}

/// 图片资源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageSource {
    /// base64 编码的图片数据
    pub data: String,
    /// MIME 类型，如 "image/png"
    pub media_type: String,
}

/// LLM 请求的工具调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// 内容块 — Message 和 ChatResponse 的基本组成单元。
/// 核心层极简，无 provider 特有标记。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    Image { source: ImageSource },
    ToolCall(ToolCall),
}

impl ContentBlock {
    pub fn text(s: String) -> Self {
        ContentBlock::Text(TextBlock { text: s })
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text(block) => Some(&block.text),
            _ => None,
        }
    }
}

/// 对话中的单条消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    System {
        content: Vec<ContentBlock>,
    },
    User {
        content: Vec<ContentBlock>,
    },
    Assistant {
        content: Vec<ContentBlock>,
    },
    ToolResult {
        tool_call_id: String,
        content: Vec<ContentBlock>,
    },
}

impl Message {
    /// 返回内容块的引用（用于 provider 适配器序列化）
    pub fn content(&self) -> &Vec<ContentBlock> {
        match self {
            Message::System { content }
            | Message::User { content }
            | Message::Assistant { content }
            | Message::ToolResult { content, .. } => content,
        }
    }

    /// 提取所有 ToolCall（仅 Assistant 消息包含）
    pub fn extract_tool_calls(&self) -> Vec<ToolCall> {
        match self {
            Message::Assistant { content } => content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolCall(tc) = b {
                        Some(tc.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// 便捷函数：创建纯文本块
pub fn text_block(s: String) -> Vec<ContentBlock> {
    vec![ContentBlock::text(s)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_block_text() {
        let block = ContentBlock::text("hello".to_string());
        assert_eq!(block.as_text(), Some("hello"));
    }

    #[test]
    fn test_content_block_tool_call_no_as_text() {
        let block = ContentBlock::ToolCall(ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: serde_json::json!({}),
        });
        assert_eq!(block.as_text(), None);
    }

    #[test]
    fn test_message_content() {
        let msg = Message::User {
            content: text_block("hello world".to_string()),
        };
        assert_eq!(msg.content().len(), 1);
        assert_eq!(msg.content()[0].as_text(), Some("hello world"));
    }

    #[test]
    fn test_message_extract_tool_calls() {
        let tc = ToolCall {
            id: "1".into(),
            name: "test".into(),
            arguments: serde_json::json!({}),
        };
        let msg = Message::Assistant {
            content: vec![ContentBlock::ToolCall(tc.clone())],
        };
        let calls = msg.extract_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "test");
    }
}
