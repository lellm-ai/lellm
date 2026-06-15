//! OpenAI 兼容协议适配器。
//!
//! 覆盖 OpenAI、NVIDIA、DeepSeek、VLLM、LLaMA 等使用 OpenAI 兼容接口的 provider。

use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, ReasoningConfig, TextBlock,
    ThinkingBlock, TokenUsage, ToolCall, ToolChoice, ToolDefinition,
};
use std::borrow::Cow;

use super::codec::{
    AuthStyle, Capabilities, ChatCodec, CodecRequest, ModelCapabilities, ProviderMeta, StreamChunk,
    StreamParseResult, ToolCallDelta,
};
use super::stream::sse_frame::SseFrame;

/// OpenAI 兼容适配器 — 一个实现覆盖所有 OpenAI 兼容 provider。
#[derive(Debug, Clone)]
pub struct OpenAICompatCodec {
    /// Provider 标识
    pub provider_id: String,
}

impl OpenAICompatCodec {
    pub fn openai() -> Self {
        Self {
            provider_id: "openai".into(),
        }
    }

    pub fn nvidia() -> Self {
        Self {
            provider_id: "nvidia".into(),
        }
    }

    pub fn deepseek() -> Self {
        Self {
            provider_id: "deepseek".into(),
        }
    }

    pub fn vllm() -> Self {
        Self {
            provider_id: "vllm".into(),
        }
    }

    pub fn llama() -> Self {
        Self {
            provider_id: "llama".into(),
        }
    }
}

// ── ProviderMeta ──

impl ProviderMeta for OpenAICompatCodec {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn default_base_url(&self) -> &'static str {
        match self.provider_id.as_str() {
            "openai" => "https://api.openai.com/v1",
            "deepseek" => "https://api.deepseek.com/v1",
            "nvidia" => "https://integrate.api.nvidia.com/v1",
            "vllm" => "http://localhost:8000/v1",
            "llama" => "http://localhost:8080/v1",
            _ => "http://localhost",
        }
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::Bearer
    }
}

// ── ChatCodec ──

impl ChatCodec for OpenAICompatCodec {
    fn encode(&self, req: &ChatRequest, stream: bool) -> Result<CodecRequest, LlmError> {
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(serialize_openai_message)
            .collect::<Result<_, _>>()?;

        let mut body = serde_json::Map::new();
        body.insert("model".into(), req.model.clone().into());
        body.insert(
            "messages".into(),
            serde_json::to_value(messages).map_err(|e| LlmError::Parse {
                detail: format!("Failed to serialize messages: {}", e),
            })?,
        );
        if stream {
            body.insert("stream".into(), true.into());
            // 流式模式下请求 usage 数据（OpenAI 默认不返回）
            body.insert(
                "stream_options".into(),
                serde_json::json!({ "include_usage": true }),
            );
        }
        if let Some(temp) = req.temperature {
            body.insert("temperature".into(), temp.into());
        }
        if let Some(max_tokens) = req.max_tokens {
            body.insert("max_tokens".into(), max_tokens.into());
        }
        if let Some(top_p) = req.top_p {
            body.insert("top_p".into(), top_p.into());
        }
        if let Some(seed) = req.seed {
            body.insert("seed".into(), seed.into());
        }
        if let Some(ref tool_choice) = req.tool_choice {
            body.insert(
                "tool_choice".into(),
                serialize_openai_tool_choice(tool_choice),
            );
        }
        if let Some(ref stop_sequences) = req.stop_sequences {
            body.insert("stop".into(), serde_json::to_value(stop_sequences).unwrap());
        }
        // 推理配置映射 — 按 Provider 协议差异化序列化
        // None → 不插入字段（Provider 默认行为）
        if let Some(ref reasoning) = req.reasoning {
            for (key, value) in serialize_reasoning_fields(&self.provider_id, reasoning) {
                body.insert(key, value);
            }
        }
        // 推理 Token 上限 — 按 Provider 协议差异化映射
        if let Some(tokens) = req.max_reasoning_tokens
            && let Some((key, value)) = serialize_max_reasoning_tokens(&self.provider_id, tokens)
        {
            body.insert(key, value);
        }
        if let Some(ref tools) = req.tools {
            body.insert(
                "tools".into(),
                serde_json::to_value(serialize_openai_tools(tools)).map_err(|e| {
                    LlmError::Parse {
                        detail: format!("Failed to serialize tools: {}", e),
                    }
                })?,
            );
        }
        // Provider 特有参数（extra 最后合并，允许覆盖标准字段）
        if let Some(ref extra) = req.extra {
            for (k, v) in extra {
                body.insert(k.clone(), v.clone());
            }
        }

        let body_bytes = serde_json::to_vec(&body).map_err(|e| LlmError::Parse {
            detail: format!("Failed to serialize request body: {}", e),
        })?;

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/json".parse().map_err(|_| LlmError::Parse {
                detail: "Invalid header value".into(),
            })?,
        );

        Ok(CodecRequest {
            path: Cow::Borrowed("/v1/chat/completions"),
            headers,
            body: Bytes::from(body_bytes),
        })
    }

    fn decode(&self, body: &[u8]) -> Result<ChatResponse, LlmError> {
        let raw: serde_json::Value = serde_json::from_slice(body).map_err(|e| LlmError::Parse {
            detail: format!("Invalid JSON: {}", e),
        })?;

        let choices = raw
            .get("choices")
            .and_then(|c| c.as_array())
            .ok_or(LlmError::Parse {
                detail: "Missing choices array".into(),
            })?;

        if choices.is_empty() {
            return Err(LlmError::Parse {
                detail: "Empty choices array".into(),
            });
        }

        let first = &choices[0];
        let message = first.get("message").ok_or(LlmError::Parse {
            detail: "Missing message in choice".into(),
        })?;

        // 解析 content（含 reasoning_content，支持 o 系列等推理模型）
        let mut content: Vec<ContentBlock> = Vec::new();
        if let Some(text) = message.get("content").and_then(|c| c.as_str())
            && !text.is_empty()
        {
            content.push(ContentBlock::Text(TextBlock {
                text: text.into(),
                cache_control: None,
            }));
        }
        // reasoning_content 独立于 content — o 系列同时返回两者
        if let Some(reasoning) = message.get("reasoning_content").and_then(|c| c.as_str())
            && !reasoning.is_empty()
        {
            content.push(ContentBlock::Thinking(ThinkingBlock {
                thinking: reasoning.into(),
                redacted: None,
            }));
        }

        // 解析 tool_calls
        if let Some(tc_arr) = message.get("tool_calls").and_then(|a| a.as_array()) {
            for tc in tc_arr {
                let id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args_str = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("{}");
                let arguments: serde_json::Value = serde_json::from_str(args_str)
                    .unwrap_or(serde_json::Value::String(args_str.into()));

                content.push(ContentBlock::ToolCall(ToolCall {
                    id,
                    name,
                    arguments,
                }));
            }
        }

        // 解析 usage
        let usage = parse_openai_usage(&raw);

        Ok(ChatResponse::new(content, usage, raw))
    }

    fn decode_sse(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError> {
        let data = &frame.data;
        if data.is_empty() {
            return Ok(StreamParseResult::empty());
        }

        if data == "[DONE]" {
            return Ok(StreamParseResult::chunk(StreamChunk::Done));
        }

        let val: serde_json::Value = serde_json::from_str(data).map_err(|e| LlmError::Parse {
            detail: format!("Invalid SSE JSON: {}", e),
        })?;

        let mut results: Vec<StreamChunk> = Vec::new();

        let choices = val.get("choices").and_then(|c| c.as_array());
        if let Some(choices) = choices {
            for choice in choices {
                let delta = choice.get("delta");
                let finish_reason = choice.get("finish_reason").and_then(|f| f.as_str());

                if let Some(d) = delta {
                    // 文本增量
                    if let Some(content_text) = d.get("content").and_then(|c| c.as_str())
                        && !content_text.is_empty()
                    {
                        results.push(StreamChunk::TextDelta(content_text.into()));
                    }

                    // 推理/思考增量（Qwen 等模型的 reasoning_content 字段）
                    if let Some(reasoning_text) =
                        d.get("reasoning_content").and_then(|c| c.as_str())
                        && !reasoning_text.is_empty()
                    {
                        results.push(StreamChunk::ThinkingDelta {
                            thinking: reasoning_text.into(),
                            redacted: None,
                        });
                    }

                    // 工具调用增量
                    if let Some(tc_arr) = d.get("tool_calls").and_then(|a| a.as_array()) {
                        for tc in tc_arr {
                            let index =
                                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            let id = tc.get("id").and_then(|v| v.as_str()).map(|s| s.into());
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.into());
                            let args_delta = tc
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            results.push(StreamChunk::ToolCallDelta(ToolCallDelta {
                                index,
                                id,
                                name,
                                arguments_delta: args_delta,
                            }));
                        }
                    }
                }

                if finish_reason.is_some() {
                    results.push(StreamChunk::Done);
                }
            }
        }

        // 解析 usage
        if let Some(usage_val) = val.get("usage") {
            let usage = TokenUsage {
                prompt_tokens: usage_val
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                completion_tokens: usage_val
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                total_tokens: usage_val
                    .get("total_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            };
            results.push(StreamChunk::Usage(usage));
        }

        if results.is_empty() {
            Ok(StreamParseResult::empty())
        } else {
            Ok(StreamParseResult { chunks: results })
        }
    }
}

// ── ModelCapabilities ──

impl ModelCapabilities for OpenAICompatCodec {
    fn capabilities_for(&self, model: &str) -> Capabilities {
        let mut caps = Capabilities::default();
        let lower = model.to_lowercase();
        // Most OpenAI models with "vision" or "4o" support image input
        if lower.contains("vision") || lower.contains("-4o") || lower.contains("gpt-4.5") {
            caps.supports_image_input = true;
        }
        // o1, o3, r1-style models support reasoning
        if lower.contains("o1-")
            || lower.contains("o3-")
            || lower.contains("-r1")
            || lower == "o1"
            || lower == "o3"
        {
            caps.supports_reasoning = true;
            caps.supports_stream_thinking = true;
        }
        // DeepSeek models support reasoning
        if self.provider_id == "deepseek" && lower.contains("r1") {
            caps.supports_reasoning = true;
            caps.supports_stream_thinking = true;
        }
        // OpenAI 兼容协议本身定义了 tool_calls 标准字段，
        // 所有通过 /v1/chat/completions 接入的模型都默认支持。
        caps.supports_tool_call = true;
        caps
    }
}

fn parse_openai_usage(raw: &serde_json::Value) -> TokenUsage {
    let usage_val = raw.get("usage");
    TokenUsage {
        prompt_tokens: usage_val
            .and_then(|u| u.get("prompt_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: usage_val
            .and_then(|u| u.get("completion_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: usage_val
            .and_then(|u| u.get("total_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    }
}

/// 将 Message 序列化为 OpenAI 格式的消息对象。
///
/// 关键：Assistant 消息中的 ToolCall 必须放入 `tool_calls` 数组，不能丢失。
fn serialize_openai_message(msg: &Message) -> Result<serde_json::Value, LlmError> {
    match msg {
        Message::System { content } => {
            let mut map = serde_json::Map::new();
            map.insert("role".into(), "system".into());
            map.insert(
                "content".into(),
                serialize_openai_text_blocks(content).into(),
            );
            Ok(serde_json::Value::Object(map))
        }
        Message::User { content } => {
            let mut map = serde_json::Map::new();
            map.insert("role".into(), "user".into());
            map.insert("content".into(), serialize_openai_content_blocks(content)?);
            Ok(serde_json::Value::Object(map))
        }
        Message::Assistant { content } => {
            let mut map = serde_json::Map::new();
            map.insert("role".into(), "assistant".into());

            // 提取文本（Thinking 被有意忽略 — OpenAI 不支持 thinking blocks）
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.to_string()),
                    ContentBlock::Thinking(_) => None, // OpenAI 不支持，静默跳过
                    _ => None,
                })
                .collect();
            if !text.is_empty() {
                map.insert("content".into(), text.into());
            }

            // 提取 ToolCall → tool_calls 数组
            let tool_calls: Vec<_> = content
                .iter()
                .filter_map(|b| {
                    if let ContentBlock::ToolCall(tc) = b {
                        Some(serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments.to_string()
                            }
                        }))
                    } else {
                        None
                    }
                })
                .collect();
            if !tool_calls.is_empty() {
                map.insert("tool_calls".into(), serde_json::Value::Array(tool_calls));
            }

            Ok(serde_json::Value::Object(map))
        }
        Message::ToolResult {
            tool_call_id,
            is_error: _,
            content,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("role".into(), "tool".into());
            map.insert("tool_call_id".into(), tool_call_id.clone().into());
            map.insert(
                "content".into(),
                content
                    .iter()
                    .filter_map(|b| b.as_text().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into(),
            );
            // NOTE: OpenAI API supports "content" field for tool results.
            // is_error is implicitly conveyed via the error message content.
            Ok(serde_json::Value::Object(map))
        }
    }
}

/// 将 ContentBlock 序列化为 OpenAI user 消息的 content（当前只支持 Text）
fn serialize_openai_content_blocks(blocks: &[ContentBlock]) -> Result<serde_json::Value, LlmError> {
    // v0.1: 只支持纯文本 user 消息，Image 明确报错
    for block in blocks {
        if matches!(block, ContentBlock::Image { .. }) {
            return Err(LlmError::UnsupportedFeature {
                feature: "Image in user messages (OpenAI adapter)".into(),
            });
        }
    }
    let text: String = blocks
        .iter()
        .filter_map(|b| b.as_text().map(|s| s.to_string()))
        .collect();
    Ok(serde_json::json!(text))
}

/// 将 ContentBlock 中的文本拼接为字符串（用于 System 消息）
fn serialize_openai_text_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| b.as_text().map(|s| s.to_string()))
        .collect::<Vec<_>>()
        .join("")
}

/// 将 ToolDefinition 数组序列化为 OpenAI 格式的工具列表。
///
/// OpenAI 要求每个工具都用 `{"type": "function", "function": {...}}` 包装。
fn serialize_openai_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters
                }
            })
        })
        .collect()
}

/// 将 ToolChoice 序列化为 OpenAI 格式。
fn serialize_openai_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Tool { name } => {
            serde_json::json!({"type": "function", "function": {"name": name}})
        }
        ToolChoice::Any => "required".into(),
    }
}

/// 将 ReasoningConfig 序列化为 Provider 特定的 JSON 字段。
///
/// 不同 Provider 对推理控制的协议不同：
/// - **DeepSeek**:
///   - `Disabled` → `enable_thinking: false`
///   - `Low/Medium/High` → `reasoning_effort: <level>`
/// - **llama.cpp**:
///   - `Disabled` → `thinking: false`
///   - `Low/Medium/High` → `reasoning_effort: <level>`
/// - **OpenAI / NVIDIA / vLLM / Anthropic**: `Disabled` → 不插字段（默认行为）
fn serialize_reasoning_fields(
    provider_id: &str,
    config: &ReasoningConfig,
) -> Vec<(String, serde_json::Value)> {
    match provider_id {
        "deepseek" => match config {
            ReasoningConfig::Disabled => {
                vec![("enable_thinking".into(), serde_json::Value::Bool(false))]
            }
            level => {
                vec![(
                    "reasoning_effort".into(),
                    serde_json::Value::String(openai_reasoning_effort(level)),
                )]
            }
        },
        "llama" => match config {
            ReasoningConfig::Disabled => {
                vec![(
                    "reasoning".into(),
                    serde_json::Value::String("off".to_string()),
                )]
            }
            _ => vec![],
        },
        // OpenAI, NVIDIA, vLLM — Disabled 不插字段，其余映射 reasoning_effort
        _ => match config {
            ReasoningConfig::Disabled => vec![],
            level => vec![(
                "reasoning_effort".into(),
                serde_json::Value::String(openai_reasoning_effort(level)),
            )],
        },
    }
}

/// 将 ReasoningConfig 等级映射为 OpenAI 标准 reasoning_effort 字符串。
///
/// **注意：** `Disabled` 不会传入此函数 — 调用方在各 Provider 分支已处理。
fn openai_reasoning_effort(config: &ReasoningConfig) -> String {
    match config {
        ReasoningConfig::Low => "low".into(),
        ReasoningConfig::Medium => "medium".into(),
        ReasoningConfig::High => "high".into(),
        ReasoningConfig::Disabled => unreachable!("Disabled should be handled by caller"),
    }
}

/// 将 max_reasoning_tokens 映射为 Provider 协议特定字段。
///
/// Adapter 映射：
/// - DeepSeek: `max_thinking_tokens`（DeepSeek 支持 thinking budget）
/// - OpenAI / NVIDIA / vLLM / 其他: `None`（无直接对应字段）
fn serialize_max_reasoning_tokens(
    provider_id: &str,
    tokens: u32,
) -> Option<(String, serde_json::Value)> {
    match provider_id {
        "deepseek" => Some((
            "max_thinking_tokens".into(),
            serde_json::Value::Number(tokens.into()),
        )),
        // OpenAI, NVIDIA, vLLM, llama — 无直接对应字段
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lellm_core::{CacheControl, TextBlock};

    #[test]
    fn test_tool_cache_control_ignored() {
        let tools = vec![ToolDefinition {
            name: "search".into(),
            description: "Search".into(),
            parameters: serde_json::json!({"type": "object"}),
            cache_control: Some(CacheControl::Breakpoint),
        }];
        let result = serialize_openai_tools(&tools);
        assert_eq!(result.len(), 1);
        // cache_control 不应出现在 OpenAI 输出中
        assert!(result[0].get("cache_control").is_none());
        assert_eq!(result[0]["function"]["name"], "search");
    }

    #[test]
    fn test_text_block_cache_control_ignored_in_system() {
        let blocks = vec![ContentBlock::Text(TextBlock {
            text: "system prompt".into(),
            cache_control: Some(CacheControl::Breakpoint),
        })];
        let text = serialize_openai_text_blocks(&blocks);
        // OpenAI 只取文本，忽略 cache_control
        assert_eq!(text, "system prompt");
    }

    #[test]
    fn test_text_block_cache_control_ignored_in_user() {
        let blocks = vec![ContentBlock::Text(TextBlock {
            text: "hello".into(),
            cache_control: Some(CacheControl::Breakpoint),
        })];
        let result = serialize_openai_content_blocks(&blocks).unwrap();
        // OpenAI 只取文本，忽略 cache_control
        assert_eq!(result, "hello");
    }
}
