//! MCP Integration 测试 — Transport, Client, Bridge
//!
//! 使用 MockTransport 模拟 MCP Server 行为。
//!
//! NOTE: `McpClient::with_transport` 内部使用了 `blocking_lock()`，
//! 在异步运行时中会 panic。因此 Client 和 Bridge 测试使用
//! `tokio::task::block_in_place` 来规避，或直接测试 Transport 层。

use async_trait::async_trait;
use lellm_mcp::ToolCatalog;
use lellm_mcp::protocol::{
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, JsonRpcResult, McpError, ServerError,
    TransportError,
};
use lellm_mcp::transport::{ConnectionState, McpTransport};
use std::sync::Arc;

// ============================================================================
// MockTransport — 可配置的测试用 Transport
// ============================================================================

#[derive(Default)]
struct MockBehavior {
    connect_ok: bool,
    responses: Vec<Result<serde_json::Value, &'static str>>,
    notifications: Vec<JsonRpcNotification>,
    resp_idx: std::sync::atomic::AtomicU64,
}

struct MockTransport {
    behavior: Arc<std::sync::Mutex<MockBehavior>>,
    state_tx: tokio::sync::watch::Sender<ConnectionState>,
    notif_tx: tokio::sync::broadcast::Sender<JsonRpcNotification>,
}

impl MockTransport {
    fn new() -> Self {
        let (state_tx, _) = tokio::sync::watch::channel(ConnectionState::Disconnected);
        let (notif_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            behavior: Arc::new(std::sync::Mutex::new(MockBehavior::default())),
            state_tx,
            notif_tx,
        }
    }

    fn connect_ok(&mut self, ok: bool) -> &mut Self {
        self.behavior.lock().unwrap().connect_ok = ok;
        self
    }

    fn add_success(&mut self, result: serde_json::Value) -> &mut Self {
        self.behavior.lock().unwrap().responses.push(Ok(result));
        self
    }

    fn add_error(&mut self, err: &'static str) -> &mut Self {
        self.behavior.lock().unwrap().responses.push(Err(err));
        self
    }

    fn make_response(id: u64, result: serde_json::Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: JsonRpcResult::Success(result),
        }
    }
}

#[async_trait]
impl McpTransport for MockTransport {
    async fn connect(&mut self) -> Result<(), McpError> {
        let ok = {
            let mut b = self.behavior.lock().unwrap();
            let ok = b.connect_ok;
            if ok {
                b.notifications.drain(..).for_each(|n| {
                    let _ = self.notif_tx.send(n);
                });
            }
            ok
        };
        if ok {
            self.state_tx.send_replace(ConnectionState::Ready);
            Ok(())
        } else {
            self.state_tx.send_replace(ConnectionState::Disconnected);
            Err(McpError::Transport(TransportError::Disconnected))
        }
    }

    async fn request(&self, req: JsonRpcRequest) -> Result<JsonRpcResponse, McpError> {
        let idx = self
            .behavior
            .lock()
            .unwrap()
            .resp_idx
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst) as usize;
        let result = {
            let b = self.behavior.lock().unwrap();
            b.responses.get(idx).cloned()
        };
        match result {
            Some(Ok(v)) => Ok(Self::make_response(req.id, v)),
            Some(Err(msg)) => match msg {
                "timeout" => Err(McpError::Transport(TransportError::Timeout)),
                "disconnected" => Err(McpError::Transport(TransportError::Disconnected)),
                "server_error" => Err(McpError::Server(ServerError {
                    code: -1,
                    message: msg.to_string(),
                })),
                _ => Err(McpError::Protocol(msg.to_string())),
            },
            None => Err(McpError::Transport(TransportError::Disconnected)),
        }
    }

    fn subscribe_notifications(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<JsonRpcNotification>> {
        Some(self.notif_tx.subscribe())
    }

    async fn close(&mut self) -> Result<(), McpError> {
        let _ = self.state_tx.send(ConnectionState::Closed);
        Ok(())
    }

    fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.state_tx.subscribe()
    }
}

// ============================================================================
// Helper: 创建 McpClient
// ============================================================================

fn create_client(transport: MockTransport) -> lellm_mcp::McpClient {
    lellm_mcp::McpClient::with_transport(transport)
}

// ============================================================================
// ConnectionState 测试
// ============================================================================

#[test]
fn test_connection_state_allows_request() {
    assert!(!ConnectionState::Disconnected.allows_request());
    assert!(!ConnectionState::Connecting.allows_request());
    assert!(!ConnectionState::Initializing.allows_request());
    assert!(ConnectionState::Ready.allows_request());
    assert!(!ConnectionState::Closed.allows_request());
}

#[test]
fn test_connection_state_display() {
    assert_eq!(format!("{}", ConnectionState::Disconnected), "Disconnected");
    assert_eq!(format!("{}", ConnectionState::Connecting), "Connecting");
    assert_eq!(format!("{}", ConnectionState::Initializing), "Initializing");
    assert_eq!(format!("{}", ConnectionState::Ready), "Ready");
    assert_eq!(format!("{}", ConnectionState::Closed), "Closed");
}

#[test]
fn test_connection_state_default() {
    assert_eq!(ConnectionState::default(), ConnectionState::Disconnected);
}

// ============================================================================
// MockTransport 基础测试（直接测试 Transport，不经过 Client）
// ============================================================================

#[tokio::test]
async fn test_transport_connect_ok() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true);
    assert!(transport.connect().await.is_ok());
}

#[tokio::test]
async fn test_transport_state_after_connect() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true);
    transport.connect().await.unwrap();

    let state = *transport.state().borrow();
    assert_eq!(state, ConnectionState::Ready);
}

#[tokio::test]
async fn test_transport_connect_fail() {
    let mut transport = MockTransport::new();
    transport.connect_ok(false);
    let result = transport.connect().await;
    assert!(result.is_err());
    assert!(matches!(
        result,
        Err(McpError::Transport(TransportError::Disconnected))
    ));
}

#[tokio::test]
async fn test_transport_request_success() {
    let mut transport = MockTransport::new();
    transport
        .connect_ok(true)
        .add_success(serde_json::json!({"status": "ok"}));
    transport.connect().await.unwrap();

    let req = JsonRpcRequest::new_for_test(1, "ping", None);
    let result = transport.request(req).await.unwrap();
    assert_eq!(result.id, 1);
    if let JsonRpcResult::Success(v) = &result.result {
        assert_eq!(v, &serde_json::json!({"status": "ok"}));
    } else {
        panic!("expected success");
    }
}

#[tokio::test]
async fn test_transport_request_timeout() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_error("timeout");
    transport.connect().await.unwrap();

    let req = JsonRpcRequest::new_for_test(1, "ping", None);
    let result = transport.request(req).await;
    assert!(matches!(
        result,
        Err(McpError::Transport(TransportError::Timeout))
    ));
}

#[tokio::test]
async fn test_transport_request_server_error() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_error("server_error");
    transport.connect().await.unwrap();

    let req = JsonRpcRequest::new_for_test(1, "ping", None);
    let result = transport.request(req).await;
    assert!(matches!(result, Err(McpError::Server(_))));
}

#[tokio::test]
async fn test_transport_close() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true);
    transport.connect().await.unwrap();
    assert!(transport.close().await.is_ok());
}

#[tokio::test]
async fn test_transport_multiple_requests() {
    let mut transport = MockTransport::new();
    transport
        .connect_ok(true)
        .add_success(serde_json::json!({"n": 1}))
        .add_success(serde_json::json!({"n": 2}))
        .add_success(serde_json::json!({"n": 3}));
    transport.connect().await.unwrap();

    for i in 1..=3 {
        let req = JsonRpcRequest::new_for_test(i, "test", None);
        let resp = transport.request(req).await.unwrap();
        if let JsonRpcResult::Success(v) = &resp.result {
            assert_eq!(v["n"], i);
        } else {
            panic!("expected success for request {}", i);
        }
    }
}

#[tokio::test]
async fn test_transport_no_response_left() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true); // no responses added
    transport.connect().await.unwrap();

    let req = JsonRpcRequest::new_for_test(1, "ping", None);
    let result = transport.request(req).await;
    // 没有预设响应 → Disconnected
    assert!(matches!(
        result,
        Err(McpError::Transport(TransportError::Disconnected))
    ));
}

// ============================================================================
// McpClient 测试（通过线程规避 blocking_lock）
// ============================================================================

#[tokio::test]
async fn test_client_connect() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true);
    let mut client = create_client(transport);
    assert!(client.connect().await.is_ok());
}

#[tokio::test]
async fn test_client_request_not_ready() {
    // 不连接直接请求，应返回 Disconnected
    let transport = MockTransport::new();
    let client = create_client(transport);
    let result: Result<serde_json::Value, _> = client.request("ping", None::<&()>).await;
    assert!(matches!(
        result,
        Err(McpError::Transport(TransportError::Disconnected))
    ));
}

#[tokio::test]
async fn test_client_request_forward() {
    let mut transport = MockTransport::new();
    transport
        .connect_ok(true)
        .add_success(serde_json::json!({}));
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let result: serde_json::Value = client.request("ping", None::<&()>).await.unwrap();
    assert_eq!(result, serde_json::json!({}));
}

#[tokio::test]
async fn test_client_close() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    assert!(client.close().await.is_ok());
}

#[tokio::test]
async fn test_client_initialize() {
    let init_result = serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "serverInfo": {"name": "test-server", "version": "1.0.0"}
    });
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_success(init_result);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let result = client.initialize().await.unwrap();
    assert_eq!(result.protocol_version, "2024-11-05");
    assert_eq!(result.server_info.name, "test-server");
    assert_eq!(result.server_info.version, "1.0.0");
}

#[tokio::test]
async fn test_client_state_after_connect() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true);
    let mut client = create_client(transport);
    client.connect().await.unwrap();

    let state = *client.state().borrow();
    assert_eq!(state, ConnectionState::Ready);
}

// ============================================================================
// McpCatalog 测试（Bridge）
// ============================================================================

#[tokio::test]
async fn test_catalog_discover_empty() {
    let list_result = serde_json::json!({"tools": []});
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_success(list_result);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let catalog = lellm_mcp::McpCatalog::from_client(client).await.unwrap();
    assert!(catalog.is_empty());
    assert_eq!(catalog.len(), 0);
}

#[tokio::test]
async fn test_catalog_discover_with_tools() {
    let list_result = serde_json::json!({
        "tools": [
            {
                "name": "search",
                "description": "搜索信息",
                "inputSchema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}}
                }
            },
            {
                "name": "calculate",
                "description": "计算表达式",
                "inputSchema": {
                    "type": "object",
                    "properties": {"expr": {"type": "string"}}
                }
            }
        ]
    });
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_success(list_result);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let catalog = lellm_mcp::McpCatalog::from_client(client).await.unwrap();
    assert_eq!(catalog.len(), 2);
    assert!(!catalog.is_empty());
}

#[tokio::test]
async fn test_catalog_snapshot_structure() {
    let list_result = serde_json::json!({
        "tools": [
            {
                "name": "echo",
                "description": "回显工具",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}}
                }
            }
        ]
    });
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_success(list_result);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let catalog = lellm_mcp::McpCatalog::from_client(client).await.unwrap();
    let snapshot = catalog.snapshot().await;

    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.has_tools());
    assert!(!snapshot.is_empty());

    let defs = snapshot.definitions();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "echo");
    assert_eq!(defs[0].description, "回显工具");
}

#[tokio::test]
async fn test_catalog_snapshot_versions_stable() {
    // 新设计：version 在快照创建时固定，不变动的 catalog 返回相同 version
    let list_result = serde_json::json!({"tools": [
        {"name": "t1", "description": "tool 1", "inputSchema": {"type": "object"}}
    ]});
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_success(list_result);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let catalog = lellm_mcp::McpCatalog::from_client(client).await.unwrap();

    let snap1 = catalog.snapshot().await;
    let snap2 = catalog.snapshot().await;
    // 未更新的 catalog 返回相同 version
    assert_eq!(snap1.version(), snap2.version());
    // 是同一个 Arc
    assert!(std::sync::Arc::ptr_eq(&snap1, &snap2));
}

#[tokio::test]
async fn test_catalog_refresh() {
    let list1 = serde_json::json!({"tools": [
        {"name": "old_tool", "description": "旧工具", "inputSchema": {"type": "object"}}
    ]});
    let list2 = serde_json::json!({"tools": [
        {"name": "new_tool", "description": "新工具", "inputSchema": {"type": "object"}}
    ]});

    let mut transport = MockTransport::new();
    transport
        .connect_ok(true)
        .add_success(list1)
        .add_success(list2);
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let catalog = lellm_mcp::McpCatalog::from_client(client.clone())
        .await
        .unwrap();
    assert_eq!(catalog.len(), 1);
    assert!(catalog.snapshot().await.get("old_tool").is_some());

    catalog.update_tools().await.unwrap();
    assert_eq!(catalog.len(), 1);
    assert!(catalog.snapshot().await.get("new_tool").is_some());
    assert!(catalog.snapshot().await.get("old_tool").is_none());
}

#[tokio::test]
async fn test_catalog_discover_error_response() {
    let mut transport = MockTransport::new();
    transport.connect_ok(true).add_error("server_error");
    let mut client = create_client(transport);
    client.connect().await.unwrap();
    let client = Arc::new(client);

    let result = lellm_mcp::McpCatalog::from_client(client).await;
    assert!(result.is_err());
    assert!(matches!(result, Err(McpError::Server(_))));
}

// ============================================================================
// 端到端流程测试
// ============================================================================

#[tokio::test]
async fn test_full_mcp_flow() {
    let init_resp = serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "mcp-server", "version": "1.0.0"}
    });
    let tools_resp = serde_json::json!({
        "tools": [
            {"name": "greet", "description": "打招呼", "inputSchema": {
                "type": "object", "properties": {"name": {"type": "string"}}
            }},
            {"name": "add", "description": "加法", "inputSchema": {
                "type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}}
            }}
        ]
    });

    let mut transport = MockTransport::new();
    transport
        .connect_ok(true)
        .add_success(init_resp)
        .add_success(tools_resp);
    let mut client = create_client(transport);

    // 1. 连接
    client.connect().await.unwrap();
    let client = Arc::new(client);

    // 2. 初始化
    let init = client.initialize().await.unwrap();
    assert_eq!(init.server_info.name, "mcp-server");

    // 3. 发现工具
    let catalog = lellm_mcp::McpCatalog::from_client(client.clone())
        .await
        .unwrap();
    assert_eq!(catalog.len(), 2);

    // 4. 快照
    let snap = catalog.snapshot().await;
    assert_eq!(snap.len(), 2);
    assert!(snap.get("greet").is_some());
    assert!(snap.get("add").is_some());

    // 5. 定义列表
    let defs = snap.definitions();
    assert_eq!(defs.len(), 2);
}
