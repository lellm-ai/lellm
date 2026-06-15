//! MCP Protocol 层测试
//!
//! 纯数据测试，无需异步运行时。
//! 覆盖序列化/反序列化、解析、错误类型、状态机等。

use lellm_mcp::protocol::{
    CallToolParams, ContentBlock, InitializeParams, JsonRpcMessage, JsonRpcNotification,
    JsonRpcRequest, McpError, NotificationKind, ImplementationInfo, methods,
    notification_methods,
};

// ============================================================================
// JsonRpcRequest 测试
// ============================================================================

#[test]
fn test_request_new() {
    let req = JsonRpcRequest::new(1, "ping", None);
    assert_eq!(req.jsonrpc, "2.0");
    assert_eq!(req.id, 1);
    assert_eq!(req.method_name, "ping");
    assert!(req.params.is_none());
}

#[test]
fn test_request_with_params() {
    let params = serde_json::json!({"key": "value"});
    let req = JsonRpcRequest::new(2, methods::TOOLS_LIST, Some(params.clone()));
    assert_eq!(req.id, 2);
    assert_eq!(req.method_name, methods::TOOLS_LIST);
    assert_eq!(req.params, Some(params));
}

#[test]
fn test_request_serialization() {
    let req = JsonRpcRequest::new(1, "ping", None);
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"jsonrpc\":\"2.0\""));
    assert!(json.contains("\"id\":1"));
    assert!(json.contains("\"method\":\"ping\""));
    // params 为 None 时不应出现在 JSON 中
    assert!(!json.contains("\"params\""));
}

#[test]
fn test_request_serialization_with_params() {
    let params = serde_json::json!({"name": "test"});
    let req = JsonRpcRequest::new(1, "tools/call", Some(params));
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"name\":\"test\""));
}

#[test]
fn test_request_roundtrip() {
    let req = JsonRpcRequest::new(42, "test-method", Some(serde_json::json!({"a": 1})));
    let json = serde_json::to_value(&req).unwrap();
    let decoded: JsonRpcRequest = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.id, 42);
    assert_eq!(decoded.method_name, "test-method");
}

// ============================================================================
// JsonRpcMessage 解析测试
// ============================================================================

#[test]
fn test_parse_request_message() {
    let json = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    let msg = JsonRpcMessage::from_json(json).unwrap();
    match msg {
        JsonRpcMessage::Request(req) => {
            assert_eq!(req.id, 1);
            assert_eq!(req.method_name, "ping");
        }
        _ => panic!("expected Request"),
    }
}

#[test]
fn test_parse_response_message_success() {
    let json = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
    let msg = JsonRpcMessage::from_json(json).unwrap();
    match msg {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.id, 1);
            assert!(matches!(
                resp.result,
                lellm_mcp::protocol::JsonRpcResult::Success(_)
            ));
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn test_parse_response_message_error() {
    let json =
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"method not found"}}"#;
    let msg = JsonRpcMessage::from_json(json).unwrap();
    match msg {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.id, 1);
            match resp.result {
                lellm_mcp::protocol::JsonRpcResult::Error(err) => {
                    assert_eq!(err.code, -32601);
                    assert_eq!(err.message, "method not found");
                }
                _ => panic!("expected Error result"),
            }
        }
        _ => panic!("expected Response"),
    }
}

#[test]
fn test_parse_notification_message() {
    let json = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    let msg = JsonRpcMessage::from_json(json).unwrap();
    match msg {
        JsonRpcMessage::Notification(_notif) => {}
        _ => panic!("expected Notification"),
    }
}

// ============================================================================
// InitializeParams 测试
// ============================================================================

#[test]
fn test_initialize_params_new() {
    let params = InitializeParams::new("2024-11-05");
    assert_eq!(params.protocol_version, "2024-11-05");
    assert_eq!(params.capabilities, serde_json::json!({}));
    assert!(params.client_info.is_none());
}

#[test]
fn test_initialize_params_with_client_info() {
    let params =
        InitializeParams::new("2024-11-05").with_client_info("lellm-mcp", "0.1.0");
    assert_eq!(params.protocol_version, "2024-11-05");
    let info = params.client_info.as_ref().unwrap();
    assert_eq!(info.name, "lellm-mcp");
    assert_eq!(info.version, "0.1.0");
}

#[test]
fn test_initialize_params_serialization() {
    let params =
        InitializeParams::new("2024-11-05").with_client_info("test-client", "1.0.0");
    let json = serde_json::to_value(&params).unwrap();
    assert_eq!(json["protocolVersion"], "2024-11-05");
    assert_eq!(json["clientInfo"]["name"], "test-client");
    assert_eq!(json["clientInfo"]["version"], "1.0.0");
}

// ============================================================================
// CallToolParams 测试
// ============================================================================

#[test]
fn test_call_tool_params_new() {
    let params = CallToolParams::new("my_tool", Some(serde_json::json!({"x": 1})));
    assert_eq!(params.name, "my_tool");
    assert!(params.arguments.is_some());
}

#[test]
fn test_call_tool_params_no_args() {
    let params = CallToolParams::new("my_tool", None);
    assert_eq!(params.name, "my_tool");
    assert!(params.arguments.is_none());
}

#[test]
fn test_call_tool_params_serialization() {
    let params = CallToolParams::new("search", Some(serde_json::json!({"q": "test"})));
    let json = serde_json::to_value(&params).unwrap();
    assert_eq!(json["name"], "search");
    assert_eq!(json["arguments"]["q"], "test");
}

#[test]
fn test_call_tool_params_skip_none_args() {
    let params = CallToolParams::new("ping", None);
    let json = serde_json::to_string(&params).unwrap();
    assert!(!json.contains("arguments"));
}

// ============================================================================
// ContentBlock 测试
// ============================================================================

#[test]
fn test_content_block_text_as_text() {
    let block = ContentBlock::Text { text: "hello world".to_string() };
    assert_eq!(block.as_text(), Some("hello world"));
}

#[test]
fn test_content_block_image_as_text() {
    let block = ContentBlock::Image {
        data: "base64data".to_string(),
        mime_type: "image/png".to_string(),
    };
    assert_eq!(block.as_text(), None);
}

#[test]
fn test_content_block_unknown_as_text() {
    let block = ContentBlock::Unknown;
    assert_eq!(block.as_text(), None);
}

#[test]
fn test_content_block_text_serialization() {
    let block = ContentBlock::Text { text: "result".to_string() };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "text");
    assert_eq!(json["text"], "result");
}

#[test]
fn test_content_block_image_serialization() {
    let block = ContentBlock::Image {
        data: "img-data".to_string(),
        mime_type: "image/jpeg".to_string(),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "image");
    assert_eq!(json["data"], "img-data");
    assert_eq!(json["mimeType"], "image/jpeg");
}

#[test]
fn test_content_block_deserialize_unknown_type() {
    let json = serde_json::json!({"type": "audio", "url": "http://example.com"});
    let block: ContentBlock = serde_json::from_value(json).unwrap();
    assert!(matches!(block, ContentBlock::Unknown));
}

// ============================================================================
// NotificationKind 测试
// ============================================================================

#[test]
fn test_notification_kind_initialized() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method_name: notification_methods::INITIALIZED.to_string(),
        params: None,
    };
    assert!(matches!(notif.kind(), NotificationKind::Initialized));
}

#[test]
fn test_notification_kind_tools_list_changed() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method_name: notification_methods::TOOLS_LIST_CHANGED.to_string(),
        params: None,
    };
    assert!(matches!(notif.kind(), NotificationKind::ToolsListChanged));
}

#[test]
fn test_notification_kind_progress() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method_name: notification_methods::PROGRESS.to_string(),
        params: Some(serde_json::json!({"progress": 50, "total": 100})),
    };
    match notif.kind() {
        NotificationKind::Progress { progress, total } => {
            assert_eq!(progress, 50);
            assert_eq!(total, Some(100));
        }
        _ => panic!("expected Progress"),
    }
}

#[test]
fn test_notification_kind_progress_no_total() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method_name: notification_methods::PROGRESS.to_string(),
        params: Some(serde_json::json!({"progress": 25})),
    };
    match notif.kind() {
        NotificationKind::Progress { progress, total } => {
            assert_eq!(progress, 25);
            assert_eq!(total, None);
        }
        _ => panic!("expected Progress"),
    }
}

#[test]
fn test_notification_kind_progress_no_params() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method_name: notification_methods::PROGRESS.to_string(),
        params: None,
    };
    match notif.kind() {
        NotificationKind::Progress { progress, total } => {
            assert_eq!(progress, 0);
            assert_eq!(total, None);
        }
        _ => panic!("expected Progress"),
    }
}

#[test]
fn test_notification_kind_other() {
    let notif = JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method_name: "custom/event".to_string(),
        params: Some(serde_json::json!({"data": "x"})),
    };
    match notif.kind() {
        NotificationKind::Other { method, params } => {
            assert_eq!(method, "custom/event");
            assert_eq!(params, Some(serde_json::json!({"data": "x"})));
        }
        _ => panic!("expected Other"),
    }
}

// ============================================================================
// McpError 测试
// ============================================================================

#[test]
fn test_mcp_error_is_retriable() {
    assert!(McpError::Disconnected.is_retriable());
    assert!(McpError::Timeout.is_retriable());
    assert!(!McpError::Protocol("bad".into()).is_retriable());
    assert!(!McpError::InvalidParams("bad".into()).is_retriable());
    assert!(!McpError::ServerError("boom".into()).is_retriable());
    assert!(!McpError::MethodNotFound("x".into()).is_retriable());
}

#[test]
fn test_mcp_error_display() {
    let err = McpError::ServerError("database down".into());
    assert_eq!(format!("{}", err), "server error: database down");

    let err = McpError::MethodNotFound("tools/list".into());
    assert_eq!(format!("{}", err), "method not found: tools/list");

    let err = McpError::Disconnected;
    assert_eq!(format!("{}", err), "connection disconnected");
}

#[test]
fn test_mcp_error_from_io() {
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
    let mcp_err = McpError::from(io_err);
    assert!(matches!(mcp_err, McpError::Io(_)));
}

// ============================================================================
// methods 常量测试
// ============================================================================

#[test]
fn test_method_constants() {
    assert_eq!(methods::INITIALIZE, "initialize");
    assert_eq!(methods::TOOLS_LIST, "tools/list");
    assert_eq!(methods::TOOLS_CALL, "tools/call");
    assert_eq!(methods::PING, "ping");
}

#[test]
fn test_notification_method_constants() {
    assert_eq!(notification_methods::INITIALIZED, "notifications/initialized");
    assert_eq!(
        notification_methods::TOOLS_LIST_CHANGED,
        "notifications/tools/list_changed"
    );
    assert_eq!(notification_methods::PROGRESS, "notifications/progress");
}

// ============================================================================
// ImplementationInfo 测试
// ============================================================================

#[test]
fn test_implementation_info_roundtrip() {
    let info = ImplementationInfo {
        name: "my-server".to_string(),
        version: "2.0.0".to_string(),
    };
    let json = serde_json::to_value(&info).unwrap();
    let decoded: ImplementationInfo = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.name, "my-server");
    assert_eq!(decoded.version, "2.0.0");
}
