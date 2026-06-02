//! 请求类型。

use serde::{Deserialize, Serialize};

/// 统一的聊天请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<crate::Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub temperature: Option<f64>,
    pub tool_choice: Option<ToolChoice>,
    pub stop_sequences: Option<Vec<String>>,
    pub prefill: Option<String>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            messages: Vec::new(),
            tools: None,
            temperature: Some(0.6),
            tool_choice: None,
            stop_sequences: None,
            prefill: None,
        }
    }
}

impl ChatRequest {
    /// 便捷构造：单条用户消息
    pub fn user_prompt(prompt: String) -> Self {
        Self {
            messages: vec![crate::Message::User {
                content: crate::text_block(prompt),
            }],
            ..Default::default()
        }
    }

    pub fn with_temperature(mut self, temp: f64) -> Self {
        self.temperature = Some(temp);
        self
    }

    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }
}

/// 工具选择策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolChoice {
    Tool { name: String },
    Any,
}

/// 工具定义（输入侧）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}
