//! 消息与内容块类型。

use crate::error::{LlmError, ToolResult};
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

    /// 从工具调用结果构建 Message::ToolResult
    ///
    /// 成功 → 文本 content，失败 → `"tool error: {e}"` 文本 content。
    /// **TODO(v0.2):** 保留错误语义，如 `is_error: bool` 或独立变体。
    pub fn tool_result(call: &ToolCall, result: &ToolResult) -> Self {
        let content_str = match result {
            Ok(s) => s.clone(),
            Err(e) => format!("tool error: {e}"),
        };
        Message::ToolResult {
            tool_call_id: call.id.clone(),
            content: text_block(content_str),
        }
    }

    /// 语义校验 — 检查 Message 变体与 ContentBlock 的合法性。
    ///
    /// v0.1 核心规则：
    /// 1. `ToolResult` 禁止包含 `ToolCall` 或 `Thinking`
    /// 2. `ToolResult.tool_call_id` 非空
    /// 3. `Assistant` 中的 `ToolCall.id` 非空
    /// 4. `User` 禁止包含 `Thinking`
    pub fn validate(&self) -> Result<(), LlmError> {
        match self {
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                if tool_call_id.is_empty() {
                    return Err(LlmError::ParseError {
                        detail: "ToolResult.tool_call_id must not be empty".into(),
                    });
                }
                for block in content {
                    match block {
                        ContentBlock::ToolCall(_) => {
                            return Err(LlmError::ParseError {
                                detail: "ToolResult must not contain ToolCall blocks".into(),
                            });
                        }
                        ContentBlock::Thinking(_) => {
                            return Err(LlmError::ParseError {
                                detail: "ToolResult must not contain Thinking blocks".into(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content } => {
                for block in content {
                    if let ContentBlock::ToolCall(tc) = block {
                        if tc.id.is_empty() {
                            return Err(LlmError::ParseError {
                                detail: "Assistant ToolCall.id must not be empty".into(),
                            });
                        }
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let ContentBlock::Thinking(_) = block {
                        return Err(LlmError::ParseError {
                            detail: "User must not contain Thinking blocks".into(),
                        });
                    }
                }
            }
            Message::System { .. } => {}
        }
        Ok(())
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

    // ─── validate() 测试 ───

    #[test]
    fn test_validate_user_ok() {
        let msg = Message::User {
            content: text_block("hello".to_string()),
        };
        assert!(msg.validate().is_ok());
    }

    #[test]
    fn test_validate_user_reject_thinking() {
        let msg = Message::User {
            content: vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "hmm".into(),
                redacted: None,
            })],
        };
        assert!(matches!(msg.validate(), Err(LlmError::ParseError { .. })));
    }

    #[test]
    fn test_validate_assistant_ok() {
        let msg = Message::Assistant {
            content: text_block("hi".to_string()),
        };
        assert!(msg.validate().is_ok());
    }

    #[test]
    fn test_validate_assistant_tool_call_empty_id() {
        let msg = Message::Assistant {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: String::new(),
                name: "test".into(),
                arguments: serde_json::json!({}),
            })],
        };
        assert!(matches!(msg.validate(), Err(LlmError::ParseError { .. })));
    }

    #[test]
    fn test_validate_tool_result_ok() {
        let msg = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            content: text_block("ok".to_string()),
        };
        assert!(msg.validate().is_ok());
    }

    #[test]
    fn test_validate_tool_result_empty_id() {
        let msg = Message::ToolResult {
            tool_call_id: String::new(),
            content: text_block("ok".to_string()),
        };
        assert!(matches!(msg.validate(), Err(LlmError::ParseError { .. })));
    }

    #[test]
    fn test_validate_tool_result_reject_tool_call() {
        let msg = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "x".into(),
                name: "y".into(),
                arguments: serde_json::json!({}),
            })],
        };
        assert!(matches!(msg.validate(), Err(LlmError::ParseError { .. })));
    }

    #[test]
    fn test_validate_tool_result_reject_thinking() {
        let msg = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            content: vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "hmm".into(),
                redacted: None,
            })],
        };
        assert!(matches!(msg.validate(), Err(LlmError::ParseError { .. })));
    }

    #[test]
    fn test_validate_system_ok() {
        let msg = Message::System {
            content: text_block("you are helpful".to_string()),
        };
        assert!(msg.validate().is_ok());
    }
}
