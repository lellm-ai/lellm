//! 请求类型。

use serde::{Deserialize, Serialize};

/// 统一的聊天请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<crate::Message>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub top_p: Option<f64>,
    pub seed: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub stop_sequences: Option<Vec<String>>,
    pub prefill: Option<String>,
    /// Provider 特有参数（如 OpenAI 的 presence_penalty），由 Adapter 自行处理。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            messages: Vec::new(),
            tools: None,
            temperature: None,
            max_tokens: None,
            top_p: None,
            seed: None,
            tool_choice: None,
            stop_sequences: None,
            prefill: None,
            extra: None,
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

    pub fn with_max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = Some(max);
        self
    }

    pub fn with_top_p(mut self, top_p: f64) -> Self {
        self.top_p = Some(top_p);
        self
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        self
    }

    /// 便捷构造：单条系统消息
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.messages.insert(
            0,
            crate::Message::System {
                content: crate::text_block(prompt),
            },
        );
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
