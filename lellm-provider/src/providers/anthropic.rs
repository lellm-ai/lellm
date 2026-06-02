//! Anthropic Provider 适配器。

use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TextBlock, TokenUsage, ToolCall,
};

use super::base::{
    HttpRequest, HttpResponse, ProviderAdapter, ProviderConfig, StreamChunk, StreamParseResult,
};

/// Anthropic 适配器。
#[derive(Debug, Clone)]
pub struct AnthropicAdapter;

impl ProviderAdapter for AnthropicAdapter {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn build_request(
        &self,
        req: &ChatRequest,
        config: &ProviderConfig,
        stream: bool,
    ) -> Result<HttpRequest, LlmError> {
        let url = format!("{}/v1/messages", config.base_url);

        // Anthropic 需要 {"role": "...", "content": [...]} 格式
        // system 消息必须放在单独的 system 字段，不能在 messages 数组中
        let mut system_text = String::new();
        let mut messages: Vec<serde_json::Map<String, serde_json::Value>> = Vec::new();

        for m in &req.messages {
            match m {
                Message::System { content: _ } => {
                    system_text = m.extract_text();
                }
                _ => {
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), m.role().into());
                    map.insert(
                        "content".into(),
                        serde_json::to_value(m.content()).map_err(|e| LlmError::ParseError {
                            detail: format!("Failed to serialize content: {}", e),
                        })?,
                    );
                    messages.push(map);
                }
            }
        }

        // 构建 Anthropic 请求 body
        let mut body = serde_json::Map::new();
        body.insert("model".into(), config.model.clone().into());
        if !system_text.is_empty() {
            body.insert("system".into(), system_text.into());
        }
        body.insert(
            "messages".into(),
            serde_json::to_value(messages).map_err(|e| LlmError::ParseError {
                detail: format!("Failed to serialize messages: {}", e),
            })?,
        );
        body.insert("max_tokens".into(), 4096u64.into());
        if stream {
            body.insert("stream".into(), true.into());
        }
        if let Some(temp) = req.temperature {
            body.insert("temperature".into(), temp.into());
        }
        if let Some(ref tools) = req.tools {
            body.insert(
                "tools".into(),
                serde_json::to_value(tools).map_err(|e| LlmError::ParseError {
                    detail: format!("Failed to serialize tools: {}", e),
                })?,
            );
        }

        let body_bytes = serde_json::to_string(&body).map_err(|e| LlmError::ParseError {
            detail: format!("Failed to serialize request body: {}", e),
        })?;

        Ok(HttpRequest {
            url,
            method: "POST".into(),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("x-api-key".into(), config.api_key.clone()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ],
            body: Some(body_bytes.into_bytes()),
            stream,
        })
    }

    fn parse_response(&self, resp: &HttpResponse) -> Result<ChatResponse, LlmError> {
        let raw: serde_json::Value =
            serde_json::from_slice(&resp.body).map_err(|e| LlmError::ParseError {
                detail: format!("Invalid JSON: {}", e),
            })?;

        let content_val =
            raw.get("content")
                .and_then(|c| c.as_array())
                .ok_or(LlmError::ParseError {
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

    fn parse_stream_chunk(&self, chunk: &[u8]) -> Result<StreamParseResult, LlmError> {
        let text = std::str::from_utf8(chunk).map_err(|e| LlmError::ParseError {
            detail: format!("Invalid UTF-8: {}", e),
        })?;

        // Anthropic SSE 格式
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with("event:") {
                continue;
            }

            let json_str = if let Some(stripped) = line.strip_prefix("data: ") {
                stripped
            } else {
                line
            };

            let json_str = json_str.trim();
            if json_str.is_empty() {
                continue;
            }

            let val: serde_json::Value =
                serde_json::from_str(json_str).map_err(|e| LlmError::ParseError {
                    detail: format!("Invalid SSE JSON: {}", e),
                })?;

            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "content_block_delta" => {
                    if let Some(delta) = val
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                        && !delta.is_empty()
                    {
                        return Ok(StreamParseResult::Chunk(StreamChunk::TextDelta(
                            delta.into(),
                        )));
                    }
                }
                "message_delta" => {
                    // 提取 usage
                    if let Some(usage_val) = val.get("usage") {
                        let usage = TokenUsage {
                            prompt_tokens: 0,
                            completion_tokens: usage_val
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32,
                            total_tokens: 0,
                        };
                        return Ok(StreamParseResult::Chunk(StreamChunk::Usage(usage)));
                    }
                    return Ok(StreamParseResult::Chunk(StreamChunk::Done));
                }
                _ => {}
            }
        }

        Ok(StreamParseResult::Empty)
    }
}
