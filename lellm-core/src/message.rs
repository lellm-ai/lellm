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

/// 工具调用结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
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
    ToolResult(ToolResult),
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

/// 缓存控制标记（provider 层使用，此处仅做前向声明）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CacheControl {
    #[serde(rename = "ephemeral_buffer")]
    EphemeralBuffer,
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
    /// 提取所有文本块拼接为字符串
    pub fn extract_text(&self) -> String {
        match self {
            Message::System { content } => Self::join_text(content),
            Message::User { content } => Self::join_text(content),
            Message::Assistant { content } => Self::join_text(content),
            Message::ToolResult { content, .. } => Self::join_text(content),
        }
    }

    fn join_text(blocks: &[ContentBlock]) -> String {
        blocks
            .iter()
            .filter_map(|b| b.as_text().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join("")
    }

    /// 提取所有 ToolCall
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
