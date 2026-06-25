//! 消息与内容块类型。

use crate::error::{ParseError, ToolResult};
use serde::{Deserialize, Serialize};

/// 缓存控制标记 — Provider 无关的语义抽象。
///
/// 由 Provider Codec 映射为各 Provider 的具体格式：
/// - Anthropic: `{"type": "ephemeral"}`
/// - OpenAI: ignore（隐式缓存）
/// - Google: ignore
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CacheControl {
    /// 缓存断点 — 标记此处为缓存边界。
    /// 业务层在稳定性递减的层边界处插入。
    Breakpoint,
}

/// 纯文本块
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextBlock {
    pub text: String,

    /// 缓存控制标记。业务层在 System prompt 的稳定性层边界处设置。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
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
    /// 创建纯文本块。接受 `&str`、`String`。
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text(TextBlock {
            text: s.into(),
            cache_control: None,
        })
    }

    /// 创建带缓存标记的文本块。
    pub fn text_with_cache(s: String, cache: CacheControl) -> Self {
        ContentBlock::Text(TextBlock {
            text: s,
            cache_control: Some(cache),
        })
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
        /// 工具执行是否失败（供 Provider API 映射，如 Anthropic `is_error: true`）
        is_error: bool,
        content: Vec<ContentBlock>,
    },
}

impl Message {
    // =======================================================================
    // 便捷构造方法 — 纯文本（最常见用法）
    // =======================================================================

    /// 便捷构造：纯文本 System 消息。
    ///
    /// ```
    /// use lellm_core::Message;
    ///
    /// // 之前：
    /// // Message::System { content: lellm_core::text_block("you are helpful".to_string()) }
    ///
    /// // 现在：
    /// let msg = Message::system_text("you are helpful");
    /// ```
    pub fn system_text(s: &str) -> Self {
        Message::System {
            content: text_block(s.to_string()),
        }
    }

    /// 便捷构造：纯文本 User 消息。
    ///
    /// ```
    /// use lellm_core::Message;
    ///
    /// let msg = Message::user_text("hello");
    /// ```
    pub fn user_text(s: &str) -> Self {
        Message::User {
            content: text_block(s.to_string()),
        }
    }

    /// 便捷构造：纯文本 Assistant 消息。
    pub fn assistant_text(s: &str) -> Self {
        Message::Assistant {
            content: text_block(s.to_string()),
        }
    }

    // =======================================================================
    // 便捷构造方法 — 多模态
    // =======================================================================

    /// 便捷构造：带图片的 User 消息（文本 + 图片）。
    ///
    /// ```
    /// use lellm_core::Message;
    ///
    /// let msg = Message::user_text_image(
    ///     "what's in this image?",
    ///     "image/png".to_string(),
    ///     "base64_encoded_data".to_string(),
    /// );
    /// ```
    pub fn user_text_image(text: &str, media_type: String, data: String) -> Self {
        Message::User {
            content: vec![
                ContentBlock::text(text),
                ContentBlock::Image {
                    source: ImageSource { data, media_type },
                },
            ],
        }
    }

    /// 便捷构造：仅图片的 User 消息。
    pub fn user_image(media_type: String, data: String) -> Self {
        Message::User {
            content: vec![ContentBlock::Image {
                source: ImageSource { data, media_type },
            }],
        }
    }

    // =======================================================================
    // 便捷构造方法 — 自定义内容块
    // =======================================================================

    /// 便捷构造：System 消息（自定义 ContentBlock）。
    pub fn system(content: Vec<ContentBlock>) -> Self {
        Message::System { content }
    }

    /// 便捷构造：User 消息（自定义 ContentBlock）。
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Message::User { content }
    }

    /// 便捷构造：Assistant 消息（自定义 ContentBlock）。
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message::Assistant { content }
    }

    /// 便捷构造：ToolResult 消息（成功）。
    pub fn tool_result_ok(call_id: impl Into<String>, content: String) -> Self {
        Message::ToolResult {
            tool_call_id: call_id.into(),
            is_error: false,
            content: text_block(content),
        }
    }

    /// 便捷构造：ToolResult 消息（失败）。
    pub fn tool_error(call_id: impl Into<String>, error: String) -> Self {
        Message::ToolResult {
            tool_call_id: call_id.into(),
            is_error: true,
            content: text_block(error),
        }
    }

    // =======================================================================
    // 访问器
    // =======================================================================

    /// 返回内容块的引用（用于 provider 适配器序列化）
    pub fn content(&self) -> &Vec<ContentBlock> {
        match self {
            Message::System { content }
            | Message::User { content }
            | Message::Assistant { content }
            | Message::ToolResult { content, .. } => content,
        }
    }

    /// 返回 ToolResult 的 tool_call_id（仅 ToolResult 变体有效，其他返回 None）
    pub fn tool_call_id(&self) -> String {
        match self {
            Message::ToolResult { tool_call_id, .. } => tool_call_id.clone(),
            _ => String::new(),
        }
    }

    /// 返回 ToolResult 的 is_error 标记（仅 ToolResult 变体有效）
    pub fn is_tool_error(&self) -> bool {
        matches!(self, Message::ToolResult { is_error: true, .. })
    }

    /// 从工具调用结果构建 Message::ToolResult
    ///
    /// 成功 → 序列化 `serde_json::Value` 为文本，`is_error: false`
    /// 失败 → `"tool error: {e}"` 文本 content，`is_error: true`
    pub fn tool_result(call: &ToolCall, result: &ToolResult) -> Self {
        let (content_str, is_error) = match result {
            Ok(v) => (
                serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
                false,
            ),
            Err(e) => (format!("tool error: {e}"), true),
        };
        Message::ToolResult {
            tool_call_id: call.id.clone(),
            is_error,
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
    pub fn validate(&self) -> Result<(), ParseError> {
        match self {
            Message::ToolResult {
                tool_call_id,
                is_error: _,
                content,
            } => {
                if tool_call_id.is_empty() {
                    return Err(ParseError {
                        detail: "ToolResult.tool_call_id must not be empty".into(),
                    });
                }
                for block in content {
                    match block {
                        ContentBlock::ToolCall(_) => {
                            return Err(ParseError {
                                detail: "ToolResult must not contain ToolCall blocks".into(),
                            });
                        }
                        ContentBlock::Thinking(_) => {
                            return Err(ParseError {
                                detail: "ToolResult must not contain Thinking blocks".into(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content } => {
                for block in content {
                    if let ContentBlock::ToolCall(tc) = block
                        && tc.id.is_empty()
                    {
                        return Err(ParseError {
                            detail: "Assistant ToolCall.id must not be empty".into(),
                        });
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let ContentBlock::Thinking(_) = block {
                        return Err(ParseError {
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
pub fn text_block(s: impl Into<String>) -> Vec<ContentBlock> {
    vec![ContentBlock::text(s)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_block_text() {
        let block = ContentBlock::text("hello");
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
        let msg = Message::user_text("hello world");
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
        assert!(matches!(msg.validate(), Err(ParseError { .. })));
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
        assert!(matches!(msg.validate(), Err(ParseError { .. })));
    }

    #[test]
    fn test_validate_tool_result_ok() {
        let msg = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            is_error: false,
            content: text_block("ok".to_string()),
        };
        assert!(msg.validate().is_ok());
    }

    #[test]
    fn test_validate_tool_result_empty_id() {
        let msg = Message::ToolResult {
            tool_call_id: String::new(),
            is_error: false,
            content: text_block("ok".to_string()),
        };
        assert!(matches!(msg.validate(), Err(ParseError { .. })));
    }

    #[test]
    fn test_validate_tool_result_reject_tool_call() {
        let msg = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            is_error: false,
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "x".into(),
                name: "y".into(),
                arguments: serde_json::json!({}),
            })],
        };
        assert!(matches!(msg.validate(), Err(ParseError { .. })));
    }

    #[test]
    fn test_validate_tool_result_reject_thinking() {
        let msg = Message::ToolResult {
            tool_call_id: "call_1".to_string(),
            is_error: false,
            content: vec![ContentBlock::Thinking(ThinkingBlock {
                thinking: "hmm".into(),
                redacted: None,
            })],
        };
        assert!(matches!(msg.validate(), Err(ParseError { .. })));
    }

    #[test]
    fn test_validate_system_ok() {
        let msg = Message::System {
            content: text_block("you are helpful".to_string()),
        };
        assert!(msg.validate().is_ok());
    }

    // ─── 便捷构造方法测试 ───

    #[test]
    fn test_convenience_system_text() {
        let msg = Message::system_text("you are helpful");
        assert!(matches!(msg, Message::System { .. }));
        assert_eq!(msg.content()[0].as_text(), Some("you are helpful"));
    }

    #[test]
    fn test_convenience_user_text() {
        let msg = Message::user_text("hello");
        assert!(matches!(msg, Message::User { .. }));
        assert_eq!(msg.content()[0].as_text(), Some("hello"));
    }

    #[test]
    fn test_convenience_assistant_text() {
        let msg = Message::assistant_text("the answer is 42");
        assert!(matches!(msg, Message::Assistant { .. }));
        assert_eq!(msg.content()[0].as_text(), Some("the answer is 42"));
    }

    #[test]
    fn test_convenience_system_content() {
        let msg = Message::system(vec![ContentBlock::text("prompt")]);
        assert!(matches!(msg, Message::System { .. }));
        assert_eq!(msg.content()[0].as_text(), Some("prompt"));
    }

    #[test]
    fn test_convenience_user_content() {
        let msg = Message::user(vec![ContentBlock::text("question")]);
        assert!(matches!(msg, Message::User { .. }));
        assert_eq!(msg.content()[0].as_text(), Some("question"));
    }

    #[test]
    fn test_convenience_tool_result_ok() {
        let msg = Message::tool_result_ok("call_1", "result data".to_string());
        assert!(matches!(msg, Message::ToolResult { .. }));
        assert!(!msg.is_tool_error());
        assert_eq!(msg.tool_call_id(), "call_1");
    }

    #[test]
    fn test_convenience_tool_error() {
        let msg = Message::tool_error("call_2", "something failed".to_string());
        assert!(matches!(msg, Message::ToolResult { .. }));
        assert!(msg.is_tool_error());
        assert_eq!(msg.tool_call_id(), "call_2");
    }

    #[test]
    fn test_content_block_text_with_string() {
        let s = String::from("dynamic");
        let block = ContentBlock::text(s);
        assert_eq!(block.as_text(), Some("dynamic"));
    }

    #[test]
    fn test_text_block_with_str() {
        let blocks = text_block("hello");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].as_text(), Some("hello"));
    }

    #[test]
    fn test_text_block_with_string() {
        let blocks = text_block(String::from("hello"));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].as_text(), Some("hello"));
    }

    // ─── 多模态便捷构造测试 ───

    #[test]
    fn test_convenience_user_text_image() {
        let msg = Message::user_text_image("what's this?", "image/png".into(), "base64data".into());
        assert!(matches!(msg, Message::User { .. }));
        assert_eq!(msg.content().len(), 2);
        assert_eq!(msg.content()[0].as_text(), Some("what's this?"));
        match &msg.content()[1] {
            ContentBlock::Image { source } => {
                assert_eq!(source.media_type, "image/png");
                assert_eq!(source.data, "base64data");
            }
            _ => panic!("expected Image block"),
        }
    }

    #[test]
    fn test_convenience_user_image() {
        let msg = Message::user_image("image/jpeg".into(), "jpgdata".into());
        assert!(matches!(msg, Message::User { .. }));
        assert_eq!(msg.content().len(), 1);
        match &msg.content()[0] {
            ContentBlock::Image { source } => {
                assert_eq!(source.media_type, "image/jpeg");
                assert_eq!(source.data, "jpgdata");
            }
            _ => panic!("expected Image block"),
        }
    }
}
