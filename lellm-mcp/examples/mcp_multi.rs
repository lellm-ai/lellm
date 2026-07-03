//! MCP Multi-Server Example — 同时连接多个 MCP 服务器
//!
//! 演示 McpMultiClient 用法：
//! - 连接多个 MCP 服务器（SSE/HTTP）
//! - 合并工具列表
//! - 工具调用自动路由到对应服务器
//!
//! 运行：
//! ```bash
//! TENCENT_MAP_KEY=your_api_key cargo run --example mcp_multi --features bridge,sse,http -p lellm-mcp
//! ```

use lellm_mcp::McpMultiClient;
use lellm_mcp::protocol::{CallToolParams, JsonRpcRequest, methods};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("TENCENT_MAP_KEY").expect("请设置环境变量 TENCENT_MAP_KEY");

    println!("=== MCP Multi-Server Example ===\n");

    let mut client = McpMultiClient::new();

    // 添加多个服务器（这里用同一个服务器演示，实际可以连接不同服务器）
    let sse_url = format!("https://mcp.map.qq.com/sse?key={}&format=0", api_key);
    client.add_sse("qq-map-sse", &sse_url).await?;

    let http_url = format!("https://mcp.map.qq.com/mcp?key={}&format=0", api_key);
    client.add_http("qq-map-http", &http_url).await?;

    // 显示工具列表
    println!("已连接服务器:");
    for (server_name, tools) in client.tool_names() {
        println!("  {} ({} 个工具)", server_name, tools.len());
        for tool in &tools {
            println!("    - {}", tool);
        }
    }
    println!("\n共 {} 个工具\n", client.total_tools());

    // 调用 geocoder 工具（会自动路由到对应的服务器）
    let addresses = vec!["陆家嘴", "天安门", "奇台"];

    for addr in &addresses {
        println!("解析: {}", addr);
        let resp = client
            .request(JsonRpcRequest::new(
                2,
                methods::TOOLS_CALL,
                Some(serde_json::to_value(CallToolParams::new(
                    "geocoder",
                    Some(serde_json::json!({ "address": addr })),
                ))?),
            ))
            .await?;

        match resp.result {
            lellm_mcp::protocol::JsonRpcResult::Success(value) => {
                let call_result: lellm_mcp::protocol::CallToolResult =
                    serde_json::from_value(value)?;
                for content in &call_result.content {
                    if let Some(text) = content.as_text() {
                        println!("  {}\n", text);
                    }
                }
            }
            e => println!("  失败: {:?}\n", e),
        }
    }

    client.close().await?;
    Ok(())
}
