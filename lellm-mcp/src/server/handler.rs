//! MCP Server 请求处理器。
//!
//! 处理 JSON-RPC 请求，支持 stdio 和 HTTP 两种传输方式。

use std::sync::Arc;

use crate::protocol::{
    CallToolParams, JsonRpcRequest, JsonRpcResponse, JsonRpcResult, ListToolsResult,
};

use super::SimpleMcp;

/// 处理 JSON-RPC 请求。
async fn handle_request(server: &SimpleMcp, req: JsonRpcRequest) -> JsonRpcResponse {
    let result = match req.method_name.as_str() {
        "initialize" => {
            // 返回服务器信息
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": server.name(),
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        }
        "tools/list" => {
            let tools = server.tool_list();
            match serde_json::to_value(ListToolsResult { tools }) {
                Ok(v) => v,
                Err(e) => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
                            code: -32603,
                            message: e.to_string(),
                            data: None,
                        }),
                    };
                }
            }
        }
        "tools/call" => {
            let params: CallToolParams = match req.params {
                Some(p) => match serde_json::from_value(p) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
                                code: -32602,
                                message: e.to_string(),
                                data: None,
                            }),
                        };
                    }
                },
                None => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
                            code: -32602,
                            message: "missing params".to_string(),
                            data: None,
                        }),
                    };
                }
            };

            let args = params.arguments.unwrap_or(serde_json::json!({}));
            match server.call_tool(&params.name, args).await {
                Ok(result) => match serde_json::to_value(result) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id,
                            result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
                                code: -32603,
                                message: e.to_string(),
                                data: None,
                            }),
                        };
                    }
                },
                Err(e) => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id,
                        result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
                            code: -32603,
                            message: e.to_string(),
                            data: None,
                        }),
                    };
                }
            }
        }
        "ping" => serde_json::json!({}),
        _ => {
            return JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: req.id,
                result: JsonRpcResult::Error(crate::protocol::JsonRpcError {
                    code: -32601,
                    message: format!("unknown method: {}", req.method_name),
                    data: None,
                }),
            };
        }
    };

    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: req.id,
        result: JsonRpcResult::Success(result),
    }
}

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
            Ok(0) => break, // EOF
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

/// 以 HTTP 模式运行服务器。
pub async fn run_http(server: Arc<SimpleMcp>, port: u16) -> Result<(), super::ServerError> {
    use axum::{Router, routing::post};

    let app = Router::new()
        .route("/mcp", post(handle_http_request))
        .route("/sse", post(handle_http_request))
        .with_state(server);

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!(addr = %addr, "MCP Server starting");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    Ok(())
}

/// 以 SSE 模式运行服务器。
///
/// SSE 端点: GET /sse — 建立 SSE 连接
/// 请求端点: POST /messages — 发送 JSON-RPC 请求
pub async fn run_sse(server: Arc<SimpleMcp>, port: u16) -> Result<(), super::ServerError> {
    use axum::{
        Router,
        routing::{get, post},
    };
    use std::sync::atomic::AtomicU64;
    use tokio::sync::broadcast;

    let (tx, _) = broadcast::channel::<String>(1024);
    let tx = Arc::new(tx);
    let session_counter = Arc::new(AtomicU64::new(1));

    let app = Router::new()
        .route("/sse", get(handle_sse_get))
        .route("/messages/{session_id}", post(handle_sse_post))
        .with_state((server, tx.clone(), session_counter.clone()));

    let addr = format!("0.0.0.0:{}", port);
    tracing::info!(addr = %addr, "MCP SSE Server starting");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    axum::serve(listener, app)
        .await
        .map_err(|e| super::ServerError::Internal(e.to_string()))?;

    Ok(())
}

/// SSE GET 请求处理 — 建立 SSE 连接。
async fn handle_sse_get(
    axum::extract::State((_server, tx, counter)): axum::extract::State<(
        Arc<SimpleMcp>,
        Arc<tokio::sync::broadcast::Sender<String>>,
        Arc<std::sync::atomic::AtomicU64>,
    )>,
) -> axum::response::sse::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::Event;
    use futures_util::StreamExt;
    use std::convert::Infallible;
    use std::sync::atomic::Ordering;

    let session_id = counter.fetch_add(1, Ordering::SeqCst);
    let rx = tx.subscribe();

    // 立即发送 endpoint 事件
    let initial_endpoint = format!("/messages/{}", session_id);

    let stream = futures_util::stream::once(async move {
        Ok::<_, Infallible>(Event::default().event("endpoint").data(initial_endpoint))
    })
    .chain(futures_util::stream::unfold(rx, move |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(msg) => {
                    if let Some((msg_session, msg_data)) = msg.split_once(':') {
                        if msg_session == session_id.to_string() {
                            let data = msg_data.strip_prefix(' ').unwrap_or(msg_data);
                            return Some((Ok(Event::default().event("message").data(data)), rx));
                        }
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

/// SSE POST 请求处理 — 接收 JSON-RPC 请求。
async fn handle_sse_post(
    axum::extract::Path(session_id): axum::extract::Path<u64>,
    axum::extract::State((server, tx, _)): axum::extract::State<(
        Arc<SimpleMcp>,
        Arc<tokio::sync::broadcast::Sender<String>>,
        Arc<std::sync::atomic::AtomicU64>,
    )>,
    axum::Json(req): axum::Json<JsonRpcRequest>,
) -> Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    let resp = handle_request(&server, req).await;
    let json = serde_json::to_string(&resp)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // 通过 broadcast 发送响应，格式: session_id:json_data
    let msg = format!("{}:{}", session_id, json);
    let _ = tx.send(msg);

    Ok(axum::http::StatusCode::ACCEPTED)
}

/// HTTP 请求处理器。
async fn handle_http_request(
    axum::extract::State(server): axum::extract::State<Arc<SimpleMcp>>,
    axum::Json(req): axum::Json<JsonRpcRequest>,
) -> Result<axum::Json<JsonRpcResponse>, (axum::http::StatusCode, String)> {
    let resp = handle_request(&server, req).await;
    Ok(axum::Json(resp))
}
