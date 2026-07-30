//! MCP Server 请求处理器。
//!
//! 处理 JSON-RPC 请求，支持 stdio 和 Streamable HTTP 两种传输方式。
//!
//! Streamable HTTP 符合 MCP 2026-07-28 规范：
//! - 单端点 POST /mcp
//! - 根据 Accept header 选择 JSON 或 SSE 响应
//! - 无状态，无需 session-id
//! - 支持 subscriptions/listen 长连接 SSE 推送
//! - 支持 server/discover 版本发现
//! - SubscriptionManager 统一管理所有 SSE 订阅者

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;

use crate::protocol::{
    CallToolParams, DiscoverParams, DiscoveryResult, ImplementationInfo, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, JsonRpcResult, ListToolsResult, SubscriptionsListenParams,
    methods,
};

use super::SimpleMcp;

/// 默认协议版本。
const PROTOCOL_VERSION: &str = "2026-07-28";

/// 通知 channel 容量。
const NOTIFICATION_BUFFER: usize = 64;

// ─── 订阅管理器 ───────────────────────────────────────────────────────

/// SSE 订阅管理器。
///
/// 维护所有活跃的 SSE 长连接订阅者，统一广播通知事件。
#[derive(Clone)]
pub struct SubscriptionManager {
    /// 广播通知给所有 SSE 订阅者。
    broadcast_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
    /// 订阅者计数（用于生成 subscription ID）。
    counter: Arc<AtomicU64>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel::<JsonRpcNotification>(NOTIFICATION_BUFFER);
        Self {
            broadcast_tx: tx,
            counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// 创建新的 SSE 订阅流。
    ///
    /// 返回 (subscription_id, stream)，stream 会持续产出 SSE 格式的 notification 事件。
    /// stream 是 'static 的，不借用任何外部数据。
    pub fn subscribe(
        &self,
        subscriptions: Vec<String>,
    ) -> (
        String,
        impl futures_util::Stream<Item = bytes::Bytes> + 'static,
    ) {
        let sub_id = format!("sub-{}", self.counter.fetch_add(1, Ordering::Relaxed));

        let subs_json = serde_json::to_value(&subscriptions).unwrap_or_default();

        tracing::info!(
            subscription_id = %sub_id,
            subscriptions = ?subscriptions,
            "New SSE subscription created"
        );

        let rx = self.broadcast_tx.subscribe();

        // 先发送 acknowledgement
        let ack = bytes::Bytes::from(format!(
            "event: acknowledged\ndata: {{\"subscriptionId\":\"{}\",\"subscriptions\":{}}}\n\n",
            sub_id, subs_json
        ));

        let stream = futures_util::stream::once(async move { ack }).chain(
            futures_util::stream::unfold(rx, move |mut rx| async move {
                loop {
                    match rx.recv().await {
                        Ok(notification) => {
                            let json = serde_json::to_string(&notification).unwrap_or_default();
                            let sse_event = format!("event: message\ndata: {json}\n\n");
                            return Some((bytes::Bytes::from(sse_event), rx));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            tracing::warn!("SSE subscriber lagged, skipping old notifications");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::info!("SSE broadcast channel closed");
                            return None;
                        }
                    }
                }
            }),
        );

        (sub_id, stream)
    }

    /// 发送通知给所有 SSE 订阅者。
    ///
    /// 当工具列表等资源发生变更时，调用此方法推送通知。
    #[allow(dead_code)]
    pub fn notify(&self, notification: JsonRpcNotification) {
        if let Err(e) = self.broadcast_tx.send(notification) {
            tracing::debug!(error = %e, "Failed to send notification (no subscribers?)");
        }
    }
}

// ─── 核心请求处理 ────────────────────────────────────────────────────

/// 处理 JSON-RPC 请求。
async fn handle_request(server: &SimpleMcp, req: JsonRpcRequest) -> JsonRpcResponse {
    let result = match req.method_name.as_str() {
        methods::INITIALIZE => {
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": { "listChanged": true },
                    "subscriptions": true
                },
                "serverInfo": {
                    "name": server.name(),
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        }
        methods::SERVER_DISCOVER => {
            let params: Option<DiscoverParams> =
                req.params.and_then(|p| serde_json::from_value(p).ok());

            let supported = vec![
                PROTOCOL_VERSION.to_string(),
                "2025-11-25".to_string(),
                "2025-03-26".to_string(),
                "2024-11-05".to_string(),
            ];

            let versions = params
                .and_then(|p| p.protocol_versions)
                .map(|client_versions| {
                    supported
                        .iter()
                        .filter(|v| client_versions.contains(v))
                        .cloned()
                        .collect()
                })
                .unwrap_or(supported);

            serde_json::to_value(DiscoveryResult {
                protocol_versions: versions,
                capabilities: serde_json::json!({
                    "tools": { "listChanged": true },
                    "subscriptions": true
                }),
                server_info: ImplementationInfo {
                    name: server.name().to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            })
            .unwrap_or_default()
        }
        methods::TOOLS_LIST => {
            let tools = server.tool_list();
            match serde_json::to_value(ListToolsResult { tools }) {
                Ok(v) => v,
                Err(e) => return error_response(req.id, -32603, e.to_string()),
            }
        }
        methods::TOOLS_CALL => {
            let params: CallToolParams = match req.params {
                Some(p) => match serde_json::from_value(p) {
                    Ok(v) => v,
                    Err(e) => return error_response(req.id, -32602, e.to_string()),
                },
                None => return error_response(req.id, -32602, "missing params".to_string()),
            };

            let args = params.arguments.unwrap_or(serde_json::json!({}));
            match server.call_tool(&params.name, args).await {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(v) => v,
                    Err(e) => return error_response(req.id, -32603, e.to_string()),
                },
                Err(e) => return error_response(req.id, -32603, e.to_string()),
            }
        }
        methods::PING => serde_json::json!({}),
        _ => {
            return error_response(
                req.id,
                -32601,
                format!("unknown method: {}", req.method_name),
            );
        }
    };

    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: req.id,
        result: JsonRpcResult::Success(result),
    }
}

/// 构造错误响应。
fn error_response(id: u64, code: i32, message: String) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
            code,
            message,
            data: None,
        }),
    }
}

// ─── Stdio Server ────────────────────────────────────────────────────

/// 以 stdio 模式运行服务器。
pub async fn run_stdio(server: &SimpleMcp) -> Result<(), super::ServerError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let server = Arc::new(server);
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<JsonRpcRequest>(line) {
                    Ok(req) => {
                        let resp = handle_request(&server, req).await;
                        let json = serde_json::to_string(&resp).unwrap_or_default();
                        stdout.write_all(json.as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                    Err(e) => {
                        eprintln!("Invalid JSON-RPC: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("Read error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

// ─── Streamable HTTP Server ──────────────────────────────────────────

/// 以 Streamable HTTP 模式运行服务器。
///
/// 单端点 POST /mcp，符合 MCP 2026-07-28 规范。
/// 根据 Accept header 选择 JSON 或 SSE 响应格式。
/// 支持 subscriptions/listen 长连接 SSE 推送。
pub async fn run_http(server: Arc<SimpleMcp>, port: u16) -> Result<(), super::ServerError> {
    use axum::{Router, routing::post};

    let subscriptions = SubscriptionManager::new();
    let state = HttpState {
        server,
        subscriptions,
    };

    let app = Router::new()
        .route("/mcp", post(handle_streamable_http))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!(
        addr = %addr,
        protocol = "streamable-http",
        version = PROTOCOL_VERSION,
        subscriptions = "enabled",
        "MCP Server starting"
    );

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    Ok(())
}

/// HTTP Server State。
#[derive(Clone)]
pub struct HttpState {
    pub server: Arc<SimpleMcp>,
    /// SSE 订阅管理器，维护所有活跃的 subscriptions/listen 长连接。
    pub subscriptions: SubscriptionManager,
}

/// Streamable HTTP 请求处理器。
///
/// - `subscriptions/listen` → 长活 SSE 流（持续推送订阅通知）
/// - 其他请求 → 根据 Accept header 选择 JSON 或 SSE 响应
async fn handle_streamable_http(
    state: axum::extract::State<HttpState>,
    headers: axum::http::HeaderMap,
    axum::Json(req): axum::Json<JsonRpcRequest>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::Response;
    use futures_util::StreamExt;

    // subscriptions/listen → 长活 SSE 流
    if req.method_name == methods::SUBSCRIPTIONS_LISTEN {
        let params: Option<SubscriptionsListenParams> =
            req.params.and_then(|p| serde_json::from_value(p).ok());
        let subs = params.map(|p| p.subscriptions).unwrap_or_default();

        let (sub_id, stream) = state.subscriptions.subscribe(subs);

        // 发送 acknowledgement 给调用者（日志记录）
        tracing::info!(subscription_id = %sub_id, "Subscription acknowledged");

        let stream = stream.map(Ok::<_, std::convert::Infallible>);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .header("X-Accel-Buffering", "no")
            .header("X-Subscription-Id", &sub_id)
            .body(Body::from_stream(stream))
            .unwrap_or_default();
    }

    // 普通请求
    let wants_sse = headers
        .get("Accept")
        .and_then(|v| v.to_str().ok())
        .map(|v: &str| v.contains("text/event-stream"))
        .unwrap_or(false);

    let resp = handle_request(&state.server, req).await;
    let json = serde_json::to_string(&resp).unwrap_or_default();

    if wants_sse {
        let sse_event = format!("event: message\ndata: {}\n\n", json);
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("X-Accel-Buffering", "no")
            .body(Body::from(sse_event))
            .unwrap_or_default()
    } else {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/json")
            .body(Body::from(json))
            .unwrap_or_default()
    }
}

// ─── SSE Server（已废弃，保留以兼容旧客户端）────────────────────────

#[allow(dead_code)]
type SseState = (
    Arc<SimpleMcp>,
    Arc<tokio::sync::broadcast::Sender<String>>,
    Arc<std::sync::atomic::AtomicU64>,
);

/// **已废弃**：HTTP+SSE 传输已于 MCP 2026-07-28 被标记为 Deprecated。
#[allow(deprecated)]
pub async fn run_sse(server: Arc<SimpleMcp>, port: u16) -> Result<(), super::ServerError> {
    use axum::{
        Router,
        routing::{get, post},
    };
    use std::sync::atomic::AtomicU64;
    use tokio::sync::broadcast;

    let (tx, _) = broadcast::channel::<String>(1024);
    let tx = Arc::new(tx);
    let counter = Arc::new(AtomicU64::new(1));

    let app = Router::new()
        .route("/sse", get(handle_sse_get))
        .route("/messages/{id}", post(handle_sse_post))
        .with_state((server, tx.clone(), counter.clone()));

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!(addr = %addr, "MCP SSE Server starting (deprecated)");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    Ok(())
}

#[allow(dead_code)]
async fn handle_sse_get(
    axum::extract::State((_server, tx, counter)): axum::extract::State<SseState>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::Event;
    use futures_util::StreamExt;
    use std::convert::Infallible;
    use std::sync::atomic::Ordering;

    let sid = counter.fetch_add(1, Ordering::SeqCst);
    let rx = tx.subscribe();
    let endpoint = format!("/messages/{}", sid);

    let stream = futures_util::stream::once(async move {
        Ok::<_, Infallible>(Event::default().event("endpoint").data(endpoint))
    })
    .chain(futures_util::stream::unfold(rx, move |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if let Some((s, d)) = msg.split_once(':')
                        && s == sid.to_string()
                    {
                        return Some((
                            Ok(Event::default()
                                .event("message")
                                .data(d.strip_prefix(' ').unwrap_or(d))),
                            rx,
                        ));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        None
    }));

    axum::response::sse::Sse::new(stream)
}

#[allow(dead_code)]
async fn handle_sse_post(
    axum::extract::Path(sid): axum::extract::Path<u64>,
    axum::extract::State((server, tx, _)): axum::extract::State<SseState>,
    axum::Json(req): axum::Json<JsonRpcRequest>,
) -> Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    let resp = handle_request(&server, req).await;
    let json = serde_json::to_string(&resp)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = tx.send(format!("{}:{}", sid, json));
    Ok(axum::http::StatusCode::ACCEPTED)
}
