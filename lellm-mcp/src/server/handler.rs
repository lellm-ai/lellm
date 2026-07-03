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
                    "name": "lellm-mcp-server",
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

/// HTTP 请求处理器。
async fn handle_http_request(
    axum::extract::State(server): axum::extract::State<Arc<SimpleMcp>>,
    axum::Json(req): axum::Json<JsonRpcRequest>,
) -> Result<axum::Json<JsonRpcResponse>, (axum::http::StatusCode, String)> {
    let resp = handle_request(&server, req).await;
    Ok(axum::Json(resp))
}
