//! OpenAI 兼容协议适配器。
//!
//! 覆盖 OpenAI、NVIDIA、DeepSeek、VLLM、LLaMA 等使用 OpenAI 兼容接口的 provider。

use bytes::Bytes;
use http::HeaderMap;
use lellm_core::{
    ChatRequest, ChatResponse, ContentBlock, LlmError, Message, TextBlock, TokenUsage, ToolCall,
};
use std::borrow::Cow;

use super::base::{
    ProviderAdapter, ProviderRequest, StreamChunk, StreamParseResult, ToolCallDelta,
};
use super::stream::sse_frame::SseFrame;

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

    fn build_request(&self, req: &ChatRequest, stream: bool) -> Result<ProviderRequest, LlmError> {
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serialize_openai_message(m))
            .collect::<Result<_, _>>()?;

        let mut body = serde_json::Map::new();
        body.insert("model".into(), req.model.clone().into());
        body.insert(
            "messages".into(),
            serde_json::to_value(messages).map_err(|e| LlmError::ParseError {
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
        if let Some(ref tools) = req.tools {
            body.insert(
                "tools".into(),
                serde_json::to_value(tools).map_err(|e| LlmError::ParseError {
                    detail: format!("Failed to serialize tools: {}", e),
                })?,
            );
        }

        let body_bytes = serde_json::to_vec(&body).map_err(|e| LlmError::ParseError {
            detail: format!("Failed to serialize request body: {}", e),
        })?;

        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            "application/json"
                .parse()
                .map_err(|_| LlmError::ParseError {
                    detail: "Invalid header value".into(),
                })?,
        );

        Ok(ProviderRequest {
            path: Cow::Borrowed("/v1/chat/completions"),
            headers,
            body: Bytes::from(body_bytes),
        })
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, LlmError> {
        let raw: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| LlmError::ParseError {
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

    fn parse_sse_frame(&self, frame: &SseFrame) -> Result<StreamParseResult, LlmError> {
        let data = &frame.data;
        if data.is_empty() {
            return Ok(StreamParseResult::empty());
        }

        if data == "[DONE]" {
            return Ok(StreamParseResult::chunk(StreamChunk::Done));
        }

        let val: serde_json::Value =
            serde_json::from_str(data).map_err(|e| LlmError::ParseError {
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
