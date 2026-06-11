//! Google Gemini Provider 适配器。
//!
//! 使用 Gemini API 原生格式（非 OpenAI 兼容）。
//! Endpoint: `POST /v1beta/models/{model}:generateContent`

use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TextBlock, TokenUsage,
    ToolCall, ToolChoice,
};
use std::borrow::Cow;

use super::codec::{
    AuthStyle, Capabilities, ChatCodec, CodecRequest, ModelCapabilities, ProviderMeta, StreamChunk,
    StreamParseResult, ToolCallDelta,
};
use super::stream::sse_frame::SseFrame;

/// Google Gemini 协议编解码器。
#[derive(Debug, Clone)]
pub struct GoogleCodec;

// ── ProviderMeta ──

impl ProviderMeta for GoogleCodec {
    fn provider_id(&self) -> &'static str {
        "google"
    }

    fn default_base_url(&self) -> &'static str {
        "https://generativelanguage.googleapis.com"
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::Bearer
    }
}

// ── ChatCodec ──

impl ChatCodec for GoogleCodec {
    fn encode(&self, req: &ChatRequest, stream: bool) -> Result<CodecRequest, LlmError> {
        // Gemini: system 消息放在 system_instruction 字段，不在 contents 数组中
        let mut system_instruction: Option<serde_json::Value> = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();

        for m in &req.messages {
            match m {
                Message::System { content } => {
                    let text: String = content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        system_instruction =
                            Some(serde_json::json!({"role": "user", "parts": [{"text": text}]}));
                    }
                }
                Message::User { content } => {
                    let parts = serialize_google_parts(content)?;
                    if !parts.is_empty() {
                        contents.push(serde_json::json!({"role": "user", "parts": parts}));
                    }
                }
                Message::Assistant { content } => {
                    let parts = serialize_google_parts(content)?;
                    if !parts.is_empty() {
                        contents.push(serde_json::json!({"role": "model", "parts": parts}));
                    }
                }
                Message::ToolResult {
                    tool_call_id,
                    is_error: _,
                    content,
                } => {
                    let parts = serialize_google_tool_result_parts(content);
                    contents.push(serde_json::json!({
                        "role": "function",
                        "parts": [{
                            "functionResponse": {
                                "name": tool_call_id,
                                "response": {
                                    "name": tool_call_id,
                                    "content": parts
                                }
                            }
                        }]
                    }));
                }
            }
        }

        // 构建 Gemini 请求 body
        let mut body = serde_json::Map::new();
        if let Some(si) = system_instruction {
            body.insert("systemInstruction".into(), si);
        }
        body.insert("contents".into(), serde_json::to_value(contents).map_err(|e| LlmError::Parse {
            detail: format!("Failed to serialize contents: {}", e),
        })?);

        // generationConfig
        let mut gen_config = serde_json::Map::new();
        if let Some(temp) = req.temperature {
            gen_config.insert("temperature".into(), temp.into());
        }
        if let Some(max_tokens) = req.max_tokens {
            gen_config.insert("maxOutputTokens".into(), max_tokens.into());
        }
        if let Some(top_p) = req.top_p {
            gen_config.insert("topP".into(), top_p.into());
        }
        if let Some(seed) = req.seed {
            gen_config.insert("seed".into(), seed.into());
        }
        if let Some(ref stop_sequences) = req.stop_sequences {
            gen_config.insert("stopSequences".into(), serde_json::to_value(stop_sequences).unwrap());
        }
        // Gemini 不支持 thinking tokens，静默忽略 reasoning 配置

        if !gen_config.is_empty() {
            body.insert("generationConfig".into(), serde_json::Value::Object(gen_config));
        }

        // 工具
        if let Some(ref tools) = req.tools {
            body.insert(
                "tools".into(),
                serde_json::json!([{
                    "functionDeclarations": serialize_google_tools(tools)
                }]),
            );
        }

        // tool_choice 映射
        if let Some(ref tool_choice) = req.tool_choice {
            body.insert(
                "toolConfig".into(),
                serde_json::json!({
                    "functionCallingConfig": serialize_google_tool_choice(tool_choice)
                }),
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

        // 构建 path：/v1beta/models/{model}:generateContent
        // 流式通过 query param ?alt=sse 控制
        let path_str = if stream {
            format!("/v1beta/models/{}:generateContent?alt=sse", req.model)
        } else {
            format!("/v1beta/models/{}:generateContent", req.model)
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/json".parse().map_err(|_| LlmError::Parse {
                detail: "Invalid header value".into(),
            })?,
        );

        Ok(CodecRequest {
            path: Cow::Owned(path_str),
            headers,
            body: Bytes::from(body_bytes),
        })
    }

    fn decode(&self, body: &[u8]) -> Result<ChatResponse, LlmError> {
        let raw: serde_json::Value = serde_json::from_slice(body).map_err(|e| LlmError::Parse {
            detail: format!("Invalid JSON: {}", e),
        })?;

        // 检查 prompts/usageMetadata (safety filtering 等错误)
        if let Some(prompt_feedback) = raw.get("promptFeedback") {
            if let Some(block_reason) = prompt_feedback
                .get("blockReason")
                .and_then(|b| b.as_str())
            {
                return Err(LlmError::Provider {
                    provider: "google".into(),
                    status: Some(400),
                    code: None,
                    message: format!("Prompt blocked: {}", block_reason),
                });
            }
        }

        let candidates = raw
            .get("candidates")
            .and_then(|c| c.as_array())
            .ok_or(LlmError::Parse {
                detail: "Missing candidates array".into(),
            })?;

        if candidates.is_empty() {
            // 可能是 safety filtering
            return Err(LlmError::Provider {
                provider: "google".into(),
                status: Some(400),
                code: None,
                message: "No candidates in response (possibly safety filtered)".into(),
            });
        }

        let candidate = &candidates[0];
        let parts = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .ok_or(LlmError::Parse {
                detail: "Missing parts in candidate".into(),
            })?;

        let mut content: Vec<ContentBlock> = Vec::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    content.push(ContentBlock::Text(TextBlock { text: text.into() }));
                }
            }
            if let Some(func_call) = part.get("functionCall") {
                let name = func_call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = func_call
                    .get("args")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default()));

                content.push(ContentBlock::ToolCall(ToolCall {
                    id: name.clone(), // Gemini 没有独立的 tool_call_id，用函数名
                    name,
                    arguments: args,
                }));
            }
        }

        // 解析 usageMetadata
        let usage_val = raw.get("usageMetadata");
        let usage = TokenUsage {
            prompt_tokens: usage_val
                .and_then(|u| u.get("promptTokenCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            completion_tokens: usage_val
                .and_then(|u| u.get("candidatesTokenCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: usage_val
                .and_then(|u| u.get("totalTokenCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
        };

        Ok(ChatResponse::new(content, usage, raw))
    }

    fn decode_sse(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError> {
        let data = &frame.data;
        if data.is_empty() {
            return Ok(StreamParseResult::empty());
        }

        let val: serde_json::Value = serde_json::from_str(data).map_err(|e| LlmError::Parse {
            detail: format!("Invalid SSE JSON: {}", e),
        })?;

        // Gemini 流式返回 candidates 数组
        let candidates = val.get("candidates").and_then(|c| c.as_array());
        if let Some(candidates) = candidates {
            if !candidates.is_empty() {
                let candidate = &candidates[0];
                let content = candidate.get("content");
                if let Some(content) = content {
                    let parts = content.get("parts").and_then(|p| p.as_array());
                    if let Some(parts) = parts {
                        let mut results: Vec<StreamChunk> = Vec::new();

                        for part in parts {
                            // 文本增量
                            if let Some(text) = part.get("text").and_then(|t| t.as_str())
                                && !text.is_empty()
                            {
                                results.push(StreamChunk::TextDelta(text.into()));
                            }

                            // 工具调用增量
                            if let Some(func_call) = part.get("functionCall") {
                                let name = func_call
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let args = func_call
                                    .get("args")
                                    .map(|v| v.to_string());

                                results.push(StreamChunk::ToolCallDelta(ToolCallDelta {
                                    index: 0,
                                    id: None,
                                    name,
                                    arguments_delta: args,
                                }));
                            }
                        }

                        // finishReason 存在即表示本轮结束
                        if candidate.get("finishReason").is_some() {
                            results.push(StreamChunk::Done);
                        }

                        if !results.is_empty() {
                            return Ok(StreamParseResult { chunks: results });
                        }
                    }
                }
            }
        }

        // usageMetadata 可能在最后一个 chunk 中
        if let Some(usage_val) = val.get("usageMetadata") {
            let usage = TokenUsage {
                prompt_tokens: usage_val
                    .get("promptTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                completion_tokens: usage_val
                    .get("candidatesTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                total_tokens: usage_val
                    .get("totalTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            };
            return Ok(StreamParseResult::chunk(StreamChunk::Usage(usage)));
        }

        Ok(StreamParseResult::empty())
    }
}

// ── ModelCapabilities ──

impl ModelCapabilities for GoogleCodec {
    fn capabilities_for(&self, model: &str) -> Capabilities {
        let mut caps = Capabilities::default();
        let lower = model.to_lowercase();
        // Gemini 2.0 Flash 及 Pro 支持工具调用
        if lower.contains("gemini") {
            caps.supports_tool_call = true;
        }
        // Gemini 2.0 Pro 支持图片
        if lower.contains("pro") || lower.contains("2.0") {
            caps.supports_image_input = true;
        }
        caps
    }
}

/// 将 ContentBlock 序列化为 Gemini parts 数组。
fn serialize_google_parts(blocks: &[ContentBlock]) -> Result<Vec<serde_json::Value>, LlmError> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(tb) => {
                parts.push(serde_json::json!({"text": tb.text}));
            }
            ContentBlock::Thinking(_) => {
                // Gemini 不支持 thinking blocks，静默跳过
            }
            ContentBlock::ToolCall(tc) => {
                parts.push(serde_json::json!({
                    "functionCall": {
                        "name": tc.name,
                        "args": tc.arguments
                    }
                }));
            }
            ContentBlock::Image { source: _ } => {
                return Err(LlmError::UnsupportedFeature {
                    feature: "Image in content blocks (Google adapter)".into(),
                });
            }
        }
    }
    Ok(parts)
}

/// 将 ToolResult 的 content 序列化为 Gemini functionResponse 格式。
fn serialize_google_tool_result_parts(blocks: &[ContentBlock]) -> serde_json::Value {
    let text: String = blocks
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::json!(text)
}

/// 将 ToolDefinition 序列化为 Gemini functionDeclarations。
fn serialize_google_tools(tools: &[lellm_core::ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            let mut obj = serde_json::Map::new();
            obj.insert("name".into(), tool.name.clone().into());
            if !tool.description.is_empty() {
                obj.insert("description".into(), tool.description.clone().into());
            }
            obj.insert(
                "parameters".into(),
                tool.parameters.clone(),
            );
            serde_json::Value::Object(obj)
        })
        .collect()
}

/// 将 ToolChoice 序列化为 Gemini functionCallingConfig。
fn serialize_google_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Tool { name } => {
            serde_json::json!({"mode": "ANY", "allowedFunctionNames": [name]})
        }
        ToolChoice::Any => {
            serde_json::json!({"mode": "ANY"})
        }
    }
}
