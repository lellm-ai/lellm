//! Base provider — GenericProvider<Adapter> 两层架构。
//!
//! GenericProvider 封装通用逻辑（HTTP 发送、重试、超时、流式解析），
//! ProviderAdapter 只负责请求/响应的格式转换。

use async_trait::async_trait;
use futures_util::StreamExt;
use lellm_core::{ChatRequest, ChatResponse, LlmError, TokenUsage, ToolCall};
use std::collections::HashMap;

use crate::{LlmProvider, ProviderEvent, ProviderStream};

/// HTTP 请求（provider 构建，GenericProvider 发送）
#[derive(Debug)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub stream: bool,
}

/// HTTP 响应
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 流式 chunk — Adapter 解析协议后返回
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum StreamChunk {
    TextDelta(String),
    ToolCallDelta {
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    Usage(TokenUsage),
    Done,
}

/// 流式解析结果 — 三态
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum StreamParseResult {
    Chunk(StreamChunk),
    Empty,
    Done,
}

/// Provider 适配器 trait — 各 provider 只需实现此 trait。
#[allow(dead_code)]
pub(crate) trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn build_request(
        &self,
        req: &ChatRequest,
        config: &ProviderConfig,
        stream: bool,
    ) -> Result<HttpRequest, LlmError>;
    fn parse_response(&self, resp: &HttpResponse) -> Result<ChatResponse, LlmError>;
    fn parse_stream_chunk(&self, chunk: &[u8]) -> Result<StreamParseResult, LlmError>;
}

/// 通用 Provider，适配任何 ProviderAdapter。
///
/// Adapter 必须 Clone，以便在流式调用时克隆进 tokio::spawn。
#[allow(private_bounds)]
pub struct GenericProvider<A: ProviderAdapter> {
    adapter: A,
    client: reqwest::Client,
    config: ProviderConfig,
}

#[allow(private_bounds)]
impl<A: ProviderAdapter + Clone> GenericProvider<A> {
    pub fn new(adapter: A, config: ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .user_agent(format!("lellm/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();

        Self {
            adapter,
            client,
            config,
        }
    }

    /// 将内部 HttpRequest 转为 reqwest RequestBuilder
    fn build_reqwest(&self, http_req: &HttpRequest) -> reqwest::RequestBuilder {
        let builder = self.client.request(
            http_req.method.parse().unwrap_or(reqwest::Method::POST),
            &http_req.url,
        );
        let builder = http_req
            .headers
            .iter()
            .fold(builder, |b, (k, v)| b.header(k, v));
        match &http_req.body {
            Some(bytes) => builder.body(bytes.clone()),
            None => builder,
        }
    }

    /// 发送 reqwest Request 并返回 HttpResponse 或 LlmError
    async fn send_request(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<HttpResponse, LlmError> {
        let resp = builder.send().await.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|sv| (k.to_string(), sv.to_string())))
            .collect();
        let body = resp
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })?;

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[async_trait]
#[allow(private_bounds)]
impl<A: ProviderAdapter + Clone + 'static> LlmProvider for GenericProvider<A> {
    async fn call(&self, request: &ChatRequest) -> Result<ChatResponse, LlmError> {
        let http_req = self.adapter.build_request(request, &self.config, false)?;
        let builder = self.build_reqwest(&http_req);
        let http_resp = self.send_request(builder).await?;

        // 4xx/5xx 转为 ApiError
        if http_resp.status >= 400 {
            let body_str = String::from_utf8_lossy(&http_resp.body);
            return Err(LlmError::ApiError {
                provider: self.adapter.name().to_string(),
                status: http_resp.status,
                code: None,
                message: body_str.into_owned(),
            });
        }

        self.adapter.parse_response(&http_resp)
    }

    async fn stream(&self, request: &ChatRequest) -> Result<ProviderStream, LlmError> {
        let http_req = self.adapter.build_request(request, &self.config, true)?;
        let builder = self.build_reqwest(&http_req);

        let resp = builder.send().await.map_err(|e| LlmError::Network {
            detail: e.to_string(),
        })?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.bytes().await.map_err(|e| LlmError::Network {
                detail: e.to_string(),
            })?;
            let body_str = String::from_utf8_lossy(&body);
            return Err(LlmError::ApiError {
                provider: self.adapter.name().to_string(),
                status,
                code: None,
                message: body_str.into_owned(),
            });
        }

        let model = request.model.clone();
        let adapter = self.adapter.clone();

        // 使用 mpsc channel 桥接 reqwest Stream 到 ProviderStream
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let stream = resp.bytes_stream();
        let mut boxed_stream = Box::pin(stream);

        tokio::spawn(async move {
            let _ = tx.send(Ok(ProviderEvent::Start { model })).await;

            let mut accumulator = ToolCallAccumulator::new();
            let mut usage: Option<TokenUsage> = None;
            let mut is_done = false;

            // SSE 缓冲区 — bytes_stream() 可能截断单条 SSE 消息
            // 按行累积，只将完整行交给 adapter 解析
            let mut sse_buffer = String::new();

            while let Some(result) = boxed_stream.next().await {
                match result {
                    Ok(bytes) => {
                        let chunk_str = String::from_utf8_lossy(&bytes).to_string();
                        sse_buffer.push_str(&chunk_str);

                        // 提取所有完整行
                        loop {
                            match sse_buffer.find('\n') {
                                Some(end_pos) => {
                                    let line = sse_buffer[..=end_pos].to_string();
                                    sse_buffer.replace_range(..=end_pos, "");
                                    let line_bytes = line.as_bytes();

                                    match adapter.parse_stream_chunk(line_bytes) {
                                        Ok(StreamParseResult::Chunk(
                                            StreamChunk::TextDelta(text),
                                        )) => {
                                            let _ =
                                                tx.send(Ok(ProviderEvent::Token { token: text }))
                                                    .await;
                                        }
                                        Ok(StreamParseResult::Chunk(
                                            StreamChunk::ToolCallDelta {
                                                id,
                                                name,
                                                arguments_delta,
                                            },
                                        )) => {
                                            if let Some(ref call_id) = id {
                                                accumulator.feed(call_id, name, arguments_delta);
                                            }
                                        }
                                        Ok(StreamParseResult::Chunk(StreamChunk::Usage(u))) => {
                                            usage = Some(u);
                                        }
                                        Ok(StreamParseResult::Chunk(StreamChunk::Done))
                                        | Ok(StreamParseResult::Done) => {
                                            is_done = true;
                                        }
                                        Ok(StreamParseResult::Empty) => {}
                                        Err(e) => {
                                            let _ = tx.send(Err(e)).await;
                                            break;
                                        }
                                    }
                                }
                                None => {
                                    // 不完整行，继续累积
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(LlmError::Network {
                                detail: e.to_string(),
                            }))
                            .await;
                        break;
                    }
                }

                if is_done {
                    break;
                }
            }

            // 发送 Done 事件（无论是否正常结束，都要发送）

            let tool_calls = accumulator.finalize().unwrap_or_default();
            let _ = tx.send(Ok(ProviderEvent::Done { tool_calls, usage })).await;
        });

        let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        Ok(Box::pin(rx_stream))
    }

    fn provider_id(&self) -> &str {
        self.adapter.name()
    }
}

/// Provider 配置。
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            timeout_secs: 120,
        }
    }
}

/// ToolCall 增量组装器（GenericProvider 内部使用）
#[allow(dead_code)]
pub(crate) struct ToolCallAccumulator {
    current: HashMap<String, PendingToolCall>,
}

#[allow(dead_code)]
struct PendingToolCall {
    name: Option<String>,
    arguments: String,
}

#[allow(dead_code)]
impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self {
            current: HashMap::new(),
        }
    }

    /// 接收增量数据并组装
    pub fn feed(&mut self, id: &str, name: Option<String>, arguments_delta: String) {
        let entry = self
            .current
            .entry(id.to_string())
            .or_insert_with(|| PendingToolCall {
                name: None,
                arguments: String::new(),
            });
        if let Some(n) = name {
            entry.name = Some(n);
        }
        entry.arguments.push_str(&arguments_delta);
    }

    /// 完成组装，返回完整的 ToolCall 列表
    pub fn finalize(self) -> Result<Vec<ToolCall>, LlmError> {
        let mut result = Vec::new();
        for (id, pending) in self.current {
            let name = pending.name.unwrap_or_else(|| "unknown".to_string());
            let arguments: serde_json::Value = serde_json::from_str(&pending.arguments)
                .unwrap_or(serde_json::Value::String(pending.arguments));
            result.push(ToolCall {
                id,
                name,
                arguments,
            });
        }
        Ok(result)
    }
}
