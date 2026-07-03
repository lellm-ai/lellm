//! SimpleMcp 服务器测试。

#[cfg(feature = "server")]
mod server_tests {
    use lellm_mcp::SimpleMcp;

    #[tokio::test]
    async fn test_simple_mcp_tool_list() {
        let mut mcp = SimpleMcp::new("Test Server");

        mcp.tool("add", "Add two numbers", |args| async move {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(serde_json::json!({ "result": a + b }))
        });

        let tools = mcp.tool_list();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "add");
    }

    #[tokio::test]
    async fn test_simple_mcp_call_tool() {
        let mut mcp = SimpleMcp::new("Test Server");

        mcp.tool("add", "Add two numbers", |args| async move {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(serde_json::json!({ "result": a + b }))
        });

        let result = mcp
            .call_tool("add", serde_json::json!({ "a": 3, "b": 5 }))
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
    }

    #[tokio::test]
    async fn test_simple_mcp_tool_not_found() {
        let mcp = SimpleMcp::new("Test Server");

        let result = mcp.call_tool("nonexistent", serde_json::json!({})).await;

        // 工具不存在时返回 Err
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_simple_mcp_multiple_tools() {
        let mut mcp = SimpleMcp::new("Math Server");

        mcp.tool("add", "Add two numbers", |args| async move {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(serde_json::json!({ "result": a + b }))
        });

        mcp.tool("multiply", "Multiply two numbers", |args| async move {
            let a = args["a"].as_i64().unwrap_or(0);
            let b = args["b"].as_i64().unwrap_or(0);
            Ok(serde_json::json!({ "result": a * b }))
        });

        assert_eq!(mcp.tool_list().len(), 2);

        let result1 = mcp
            .call_tool("add", serde_json::json!({ "a": 1, "b": 2 }))
            .await
            .unwrap();
        assert!(!result1.is_error);

        let result2 = mcp
            .call_tool("multiply", serde_json::json!({ "a": 3, "b": 4 }))
            .await
            .unwrap();
        assert!(!result2.is_error);
    }
}
