//! 请求类型。

use serde::{Deserialize, Serialize};

/// 统一的聊天请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// 单次 LLM 调用的推理 Token 上限（可选，默认无限制）。
    ///
    /// 与 `max_tokens` 分离：reasoning 是模型内部推理，不计入输出预算。
    /// 透传给 Provider Adapter，由 Adapter 映射为协议特定字段。
    ///
    /// **两种语义：**
    /// - 流式: Hard limit — 达到限额当场切断 stream，省钱
    /// - 非流式: Soft limit — response 已完整返回，事后检测并标记
    ///
    /// Adapter 映射示例：
    /// - DeepSeek: `max_thinking_tokens`
    /// - OpenAI: 无直接对应，由 `reasoning` 级别间接控制
    /// - 其他: 放入 `extra` 或忽略
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_reasoning_tokens: Option<u32>,
    /// Provider 特有参数（如 OpenAI 的 presence_penalty），由 Adapter 自行处理。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Map<String, serde_json::Value>>,
}

// Default is derived - all fields have valid default values

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

    /// 设置单次调用的推理 Token 上限
    pub fn with_max_reasoning_tokens(mut self, max: u32) -> Self {
        self.max_reasoning_tokens = Some(max);
        self
    }

    /// 设置 Provider 特有参数
    pub fn with_extra(mut self, extra: serde_json::Map<String, serde_json::Value>) -> Self {
        self.extra = Some(extra);
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
///
/// Schema 由 `schemars` 在编译期生成，经 `compute_and_clean_schema` 清洗后
/// 存入 `parameters` 字段。Codec 层按 Provider 需求进行二次适配。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,

    /// 缓存控制标记。Anthropic 支持 Tool Definition 级别的缓存。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<crate::message::CacheControl>,
}

impl ToolDefinition {
    /// 克隆并设置缓存标记。
    pub fn with_cache(self, cache: crate::message::CacheControl) -> Self {
        Self {
            cache_control: Some(cache),
            ..self
        }
    }

    /// 从 `schemars::JsonSchema` 类型计算并清洗 JSON Schema。
    ///
    /// 供 `#[tool]` 宏生成的 `LazyLock` 调用，不在泛型函数中使用 `LazyLock`。
    ///
    /// **清洗规则：** 去除 `$schema`, `$id`, `title`, `description` 等根部元数据，
    /// 保留 `type`, `properties`, `required`, `definitions` 等核心 JSON Schema 字段。
    pub fn compute_and_clean_schema<S: schemars::JsonSchema>() -> serde_json::Value {
        let root = schemars::schema_for!(S);
        let val = serde_json::to_value(&root)
            .expect("Failed to serialize JsonSchema; this is a bug in schemars");
        Self::clean_schema(val)
    }

    /// 清洗 schemars 生成的 RootSchema，去除根部元数据噪音。
    ///
    /// 保留 `type`, `properties`, `required`, `definitions`, `additionalProperties`
    /// 等核心 JSON Schema 字段。Codec 层在此基础上进行 Provider 特定的二次适配。
    fn clean_schema(mut value: serde_json::Value) -> serde_json::Value {
        if let Some(obj) = value.as_object_mut() {
            // 去除标准 JSON Schema 根部的噪声元数据
            obj.remove("$schema");
            obj.remove("$id");
            obj.remove("title");
            obj.remove("description");
        }
        value
    }
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

impl ReasoningConfig {
    /// 判断是否为 Disabled
    pub fn is_disabled(self) -> bool {
        matches!(self, Self::Disabled)
    }
}
