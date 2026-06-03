//! OpenAI 兼容协议适配器。
//!
//! 覆盖 OpenAI、NVIDIA、DeepSeek、VLLM、LLaMA 等使用 OpenAI 兼容接口的 provider。

use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TextBlock, TokenUsage, ToolCall,
};

use super::base::{
    HttpRequest, HttpResponse, ProviderAdapter, ProviderConfig, StreamChunk, StreamParseResult,
    ToolCallDelta,
};

/// OpenAI 兼容适配器 — 一个实现覆盖所有 OpenAI 兼容 provider。
#[derive(Debug, Clone)]
pub struct OpenAICompatAdapter {
    /// Provider 标识
    pub provider_id: String,
}

impl OpenAICompatAdapter {
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

impl ProviderAdapter for OpenAICompatAdapter {
    fn name(&self) -> &str {
        &self.provider_id
    }

    fn build_request(
        &self,
        req: &ChatRequest,
        config: &ProviderConfig,
        stream: bool,
    ) -> Result<HttpRequest, LlmError> {
        let url = format!("{}/chat/completions", config.base_url);

        // 构建请求 body
        // OpenAI 需要 {"role": "...", "content": "..."} 格式
        // 不能直接使用 serde_json::to_value(&req.messages)（会序列化出 type 而非 role）
        let messages: Vec<serde_json::Map<String, serde_json::Value>> = req
            .messages
            .iter()
            .map(|m| match m {
                Message::System { content: _ } => {
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), "system".into());
                    map.insert("content".into(), m.extract_text().into());
                    map
                }
                Message::User { content: _ } => {
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), "user".into());
                    map.insert("content".into(), m.extract_text().into());
                    map
                }
                Message::Assistant { content: _ } => {
                    let mut map = serde_json::Map::new();
                    map.insert("role".into(), "assistant".into());
                    map.insert("content".into(), m.extract_text().into());
                    map
                }
                Message::ToolResult {
                    tool_call_id,
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
                    map
                }
            })
            .collect();

        let mut body = serde_json::Map::new();
        body.insert("model".into(), config.model.clone().into());
        body.insert(
            "messages".into(),
            serde_json::to_value(messages).map_err(|e| LlmError::ParseError {
                detail: format!("Failed to serialize messages: {}", e),
            })?,
        );
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
                ("Authorization".into(), format!("Bearer {}", config.api_key)),
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

        let choices =
            raw.get("choices")
                .and_then(|c| c.as_array())
                .ok_or(LlmError::ParseError {
                    detail: "Missing choices array".into(),
                })?;

        if choices.is_empty() {
            return Err(LlmError::ParseError {
                detail: "Empty choices array".into(),
            });
        }

        let first = &choices[0];
        let message = first.get("message").ok_or(LlmError::ParseError {
            detail: "Missing message in choice".into(),
        })?;

        // 解析 content
        let mut content: Vec<ContentBlock> = Vec::new();
        if let Some(text) = message.get("content").and_then(|c| c.as_str())
            && !text.is_empty()
        {
            content.push(ContentBlock::Text(TextBlock { text: text.into() }));
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

    fn parse_stream_chunk(&self, chunk: &[u8]) -> Result<StreamParseResult, LlmError> {
        let text = std::str::from_utf8(chunk).map_err(|e| LlmError::ParseError {
            detail: format!("Invalid UTF-8: {}", e),
        })?;

        let mut results: Vec<StreamChunk> = Vec::new();

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

            if json_str == "[DONE]" {
                return Ok(StreamParseResult::chunk(StreamChunk::Done));
            }

            let val: serde_json::Value =
                serde_json::from_str(json_str).map_err(|e| LlmError::ParseError {
                    detail: format!("Invalid SSE JSON: {}", e),
                })?;

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

                        // 工具调用增量 — 统一为 ToolCallDelta(index, id, name, arguments_delta)
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
        }

        if results.is_empty() {
            Ok(StreamParseResult::empty())
        } else {
            Ok(StreamParseResult { chunks: results })
        }
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
