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
    /// 推理配置 — 控制模型是否进行深度推理。
    ///
    /// `None` = 不干预 Provider 默认行为
    /// `Some(Disabled)` = 显式关闭推理
    /// `Some(Low/Medium/High)` = 开启对应级别的推理
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    /// 是否向客户端流式输出推理过程（仅影响流式行为）。
    ///
    /// `false`（默认）= 模型可推理，但不向消费者发射 ThinkingDelta 事件
    /// `true` = 将推理内容以 ThinkingDelta 事件流式输出
    pub stream_thinking: bool,
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
            reasoning: None,
            stream_thinking: false,
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

    /// 设置推理配置
    pub fn with_reasoning(mut self, reasoning: ReasoningConfig) -> Self {
        self.reasoning = Some(reasoning);
        self
    }

    /// 设置是否流式输出推理过程
    pub fn with_stream_thinking(mut self, stream_thinking: bool) -> Self {
        self.stream_thinking = stream_thinking;
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

/// 推理配置 — 声明式控制模型的深度推理行为。
///
/// 四值语义（Option + Enum）：
/// - `None`（未设置）= 不干预，Provider 自行决定默认行为
/// - `Some(Disabled)` = 显式关闭推理（尽最大努力）
/// - `Some(Low)` = 低推理预算（快速、轻量）
/// - `Some(Medium)` = 中等推理预算
/// - `Some(High)` = 高推理预算（深度思考）
///
/// Adapter 映射示例：
/// - OpenAI / NVIDIA / vLLM: `Disabled` → 不插字段；`Low` → "low"；`Medium` → "medium"；`High` → "high"
/// - DeepSeek: `Disabled` → `enable_thinking=false`；其余 → `reasoning_effort=<level>`
/// - llama.cpp: `Disabled` → `thinking=false`；其余 → `reasoning_effort=<level>`
/// - Anthropic: `Disabled` → 静默忽略（不支持推理配置）；其余 → `UnsupportedFeature`
/// - 不支持推理的 Provider: `Disabled` → 静默忽略；其余 → `UnsupportedFeature`
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReasoningConfig {
    /// 显式关闭推理
    Disabled,
    /// 低推理预算
    Low,
    /// 中等推理预算
    Medium,
    /// 高推理预算
    High,
}
