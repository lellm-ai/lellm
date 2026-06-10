//! Anthropic Provider 适配器。

use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, ReasoningConfig, TextBlock,
    ThinkingBlock, TokenUsage, ToolCall, ToolChoice,
};
use std::borrow::Cow;

use super::codec::{
    AuthStyle, Capabilities, ChatCodec, CodecRequest, ModelCapabilities, ProviderMeta, StreamChunk,
    StreamParseResult, ToolCallDelta,
};
use super::stream::sse_frame::SseFrame;

/// Anthropic 协议编解码器。
#[derive(Debug, Clone)]
pub struct AnthropicCodec;

// ── ProviderMeta ──

impl ProviderMeta for AnthropicCodec {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }

    fn default_base_url(&self) -> &'static str {
        "https://api.anthropic.com"
    }

    fn auth_style(&self) -> AuthStyle {
        AuthStyle::CustomHeader("x-api-key")
    }
}

// ── ChatCodec ──

impl ChatCodec for AnthropicCodec {
    fn encode(&self, req: &ChatRequest, stream: bool) -> Result<CodecRequest, LlmError> {
        // Anthropic 需要 {"role": "...", "content": [...]} 格式
        // system 消息必须放在单独的 system 字段，不能在 messages 数组中
        let mut system_text = String::new();
        let mut messages: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();

        for m in &req.messages {
            match m {
                Message::System { content } => {
                    system_text = content.iter().filter_map(|b| b.as_text()).collect();
                }
                Message::User { content } => {
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), "user".into());
                    map.insert(
                        "content".into(),
                        serialize_anthropic_content_blocks(content)?,
                    );
                    messages.push(map);
                }
                Message::Assistant { content } => {
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), "assistant".into());
                    map.insert(
                        "content".into(),
                        serialize_anthropic_content_blocks(content)?,
                    );
                    messages.push(map);
                }
                Message::ToolResult {
                    tool_call_id,
                    is_error,
                    content,
                } => {
                    // Anthropic: tool_result 是 role="user" 消息中的 content block
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), "user".into());
                    let mut block = serde_json::Map::new();
                    block.insert("type".into(), "tool_result".into());
                    block.insert("tool_use_id".into(), tool_call_id.clone().into());
                    block.insert("is_error".into(), (*is_error).into());
                    block.insert(
                        "content".into(),
                        serialize_anthropic_content_blocks(content)?,
                    );
                    map.insert(
                        "content".into(),
                        serde_json::Value::Array(vec![serde_json::Value::Object(block)]),
                    );
                    messages.push(map);
                }
            }
        }

        // 构建 Anthropic 请求 body
        let mut body = serde_json::Map::new();
        body.insert("model".into(), req.model.clone().into());
        if !system_text.is_empty() {
            body.insert("system".into(), system_text.into());
        }
        body.insert(
            "messages".into(),
            serde_json::to_value(messages).map_err(|e| LlmError::Parse {
                detail: format!("Failed to serialize messages: {}", e),
            })?,
        );
        // Anthropic 要求 max_tokens 必填，未设置时返回错误
        let max_tokens = req.max_tokens.ok_or_else(|| LlmError::InvalidRequest {
            message: "Anthropic requires max_tokens".into(),
        })?;
        body.insert("max_tokens".into(), (max_tokens as u64).into());

        // 推理配置映射 — Anthropic thinking.enabled + budget_tokens
        //
        // | ReasoningConfig | thinking 字段 | budget_tokens |
        // | Disabled        | omit          | —             |
        // | Low             | enabled       | 2048          |
        // | Medium          | enabled       | 8192          |
        // | High            | enabled       | 32768         |
        //
        // max_reasoning_tokens 存在时 → 覆盖默认 budget
        if let Some(ref reasoning) = req.reasoning {
            match reasoning {
                ReasoningConfig::Disabled => {} // 不推理，omit thinking 字段
                ReasoningConfig::Low
                | ReasoningConfig::Medium
                | ReasoningConfig::High => {
                    let default_budget = match reasoning {
                        ReasoningConfig::Low => 2048,
                        ReasoningConfig::Medium => 8192,
                        ReasoningConfig::High => 32768,
                        _ => unreachable!(),
                    };
                    let budget_tokens = req.max_reasoning_tokens.unwrap_or(default_budget) as u64;
                    body.insert(
                        "thinking".into(),
                        serde_json::json!({
                            "type": "enabled",
                            "budget_tokens": budget_tokens
                        }),
                    );
                }
            }
        }

        if stream {
            body.insert("stream".into(), true.into());
        }
        if let Some(temp) = req.temperature {
            body.insert("temperature".into(), temp.into());
        }
        if let Some(top_p) = req.top_p {
            body.insert("top_p".into(), top_p.into());
        }
        if let Some(ref tool_choice) = req.tool_choice {
            body.insert(
                "tool_choice".into(),
                serialize_anthropic_tool_choice(tool_choice),
            );
        }
        if let Some(ref stop_sequences) = req.stop_sequences {
            body.insert(
                "stop_sequences".into(),
                serde_json::to_value(stop_sequences).unwrap(),
            );
        }
        if let Some(ref tools) = req.tools {
            body.insert(
                "tools".into(),
                serde_json::to_value(tools).map_err(|e| LlmError::Parse {
                    detail: format!("Failed to serialize tools: {}", e),
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
            "anthropic-version",
            "2023-06-01".parse().map_err(|_| LlmError::Parse {
                detail: "Invalid header value".into(),
            })?,
        );

        Ok(CodecRequest {
            path: Cow::Borrowed("/v1/messages"),
            headers,
            body: Bytes::from(body_bytes),
        })
    }

    fn decode(&self, body: &[u8]) -> Result<ChatResponse, LlmError> {
        let raw: serde_json::Value = serde_json::from_slice(body).map_err(|e| LlmError::Parse {
            detail: format!("Invalid JSON: {}", e),
        })?;

        let content_val = raw
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or(LlmError::Parse {
                detail: "Missing content array".into(),
            })?;

        let mut content: Vec<ContentBlock> = Vec::new();
        for block in content_val {
            let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match block_type {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        content.push(ContentBlock::Text(TextBlock { text: text.into() }));
                    }
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = block
                        .get("input")
                        .unwrap_or(&serde_json::Value::Object(Default::default()))
                        .clone();

                    content.push(ContentBlock::ToolCall(ToolCall {
                        id,
                        name,
                        arguments: input,
                    }));
                }
                "thinking" => {
                    let thinking = block
                        .get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let redacted = block
                        .get("redacted_thinking")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    if !thinking.is_empty() || redacted.is_some() {
                        content.push(ContentBlock::Thinking(ThinkingBlock { thinking, redacted }));
                    }
                }
                _ => {}
            }
        }

        // 解析 usage
        let usage_val = raw.get("usage");
        let prompt_tokens = usage_val
            .and_then(|u| u.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let completion_tokens = usage_val
            .and_then(|u| u.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let usage = TokenUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
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

        let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "message_start" => {
                // 提取 input_tokens（流式模式下 message_start 携带 prompt_tokens）
                if let Some(usage_val) = val.get("usage") {
                    let input_tokens = usage_val
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    if input_tokens > 0 {
                        return Ok(StreamParseResult::chunk(StreamChunk::InputTokens(
                            input_tokens,
                        )));
                    }
                }
                return Ok(StreamParseResult::empty());
            }
            "content_block_start" => {
                let block = val.get("content_block").unwrap_or(&serde_json::Value::Null);
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if block_type == "tool_use" {
                    let index = val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let id = block.get("id").and_then(|v| v.as_str()).map(|s| s.into());
                    let name = block.get("name").and_then(|v| v.as_str()).map(|s| s.into());
                    return Ok(StreamParseResult::chunk(StreamChunk::ToolCallDelta(
                        ToolCallDelta {
                            index,
                            id,
                            name,
                            arguments_delta: None,
                        },
                    )));
                }
            }
            "content_block_delta" => {
                let delta = val.get("delta").unwrap_or(&serde_json::Value::Null);
                let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let index = val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                if delta_type == "text_delta" {
                    if let Some(text) = delta.get("text").and_then(|t| t.as_str())
                        && !text.is_empty()
                    {
                        return Ok(StreamParseResult::chunk(StreamChunk::TextDelta(
                            text.into(),
                        )));
                    }
                } else if delta_type == "input_json_delta" {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if partial.is_some() {
                        return Ok(StreamParseResult::chunk(StreamChunk::ToolCallDelta(
                            ToolCallDelta {
                                index,
                                id: None,
                                name: None,
                                arguments_delta: partial,
                            },
                        )));
                    }
                } else if delta_type == "thinking_delta" {
                    let thinking = delta
                        .get("thinking")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string());
                    let redacted = delta
                        .get("redacted_thinking")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string());
                    if let Some(t) = thinking {
                        return Ok(StreamParseResult::chunk(StreamChunk::ThinkingDelta {
                            thinking: t,
                            redacted,
                        }));
                    }
                    // redacted_thinking without thinking is also valid
                    if let Some(r) = redacted {
                        return Ok(StreamParseResult::chunk(StreamChunk::ThinkingDelta {
                            thinking: String::new(),
                            redacted: Some(r),
                        }));
                    }
                }
            }

            "message_delta" => {
                let mut chunks = Vec::new();

                if let Some(usage_val) = val.get("usage") {
                    let output_tokens = usage_val
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    if output_tokens > 0 {
                        chunks.push(StreamChunk::OutputTokens(output_tokens));
                    }
                }

                // message_delta 总是流中的最后一个有意义事件，附带 Done
                chunks.push(StreamChunk::Done);
                return Ok(StreamParseResult { chunks });
            }
            _ => {}
        }

        Ok(StreamParseResult::empty())
    }
}

// ── ModelCapabilities ──

impl ModelCapabilities for AnthropicCodec {
    fn capabilities_for(&self, model: &str) -> Capabilities {
        let mut caps = Capabilities::default();
        let lower = model.to_lowercase();
        if lower.contains("claude-3") || lower.contains("claude-4") {
            caps.supports_image_input = true;
        }
        // Claude 3.5 Sonnet+ 和 Claude 4 系列支持 thinking 模式
        if lower.contains("sonnet")
            || lower.contains("opus")
            || (lower.contains("claude-4") && lower.contains("sonnet"))
        {
            caps.supports_reasoning = true;
        }
        caps
    }
}

/// 将 ContentBlock 序列化为 Anthropic 格式的内容数组。
///
/// 关键映射：
/// - `Text` → `{"type": "text", "text": "..."}`
/// - `ToolCall` → `{"type": "tool_use", "id": ..., "name": ..., "input": {...}}`
/// - `Thinking` → `{"type": "thinking", "thinking": "..."}`
/// - `Image` → 暂不支持，返回 `UnsupportedFeature` 错误
fn serialize_anthropic_content_blocks(
    blocks: &[ContentBlock],
) -> Result<serde_json::Value, LlmError> {
    let arr: Vec<serde_json::Value> = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(tb) => Ok(serde_json::json!({
                "type": "text",
                "text": tb.text
            })),
            ContentBlock::Thinking(tb) => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), "thinking".into());
                obj.insert("thinking".into(), serde_json::json!(tb.thinking));
                if let Some(ref redacted) = tb.redacted {
                    obj.insert("redacted_thinking".into(), serde_json::json!(redacted));
                }
                Ok(serde_json::Value::Object(obj))
            }
            ContentBlock::ToolCall(tc) => Ok(serde_json::json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.name,
                "input": tc.arguments
            })),
            ContentBlock::Image { source: _ } => Err(LlmError::UnsupportedFeature {
                feature: "Image in content blocks (Anthropic adapter)".into(),
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(serde_json::Value::Array(arr))
}

/// 将 ToolChoice 序列化为 Anthropic 格式。
fn serialize_anthropic_tool_choice(choice: &ToolChoice) -> serde_json::Value {
    match choice {
        ToolChoice::Tool { name } => {
            serde_json::json!({"type": "tool", "name": name})
        }
        ToolChoice::Any => "any".into(),
    }
}
