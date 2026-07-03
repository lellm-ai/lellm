//! MCP Weather Example — 使用 QQ 地图 MCP 服务器查询天气 (HTTP Transport)
//!
//! 前置条件：
//! 1. 在腾讯位置服务申请 API Key: https://lbs.qq.com/service/webService/webServiceGuide/overview
//! 2. 设置环境变量: export TENCENT_MAP_KEY=your_api_key
//!
//! 运行：
//! ```bash
//! TENCENT_MAP_KEY=your_api_key cargo run --example mcp_weather_http --features http -p lellm-mcp
//! ```

use lellm_mcp::client::McpClient;
use lellm_mcp::protocol::{CallToolParams, JsonRpcRequest, methods};
use lellm_mcp::transport::{HttpConfig, HttpTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key =
        std::env::var("TENCENT_MAP_KEY").expect("请设置环境变量 TENCENT_MAP_KEY");

    let endpoint_url = format!("https://mcp.map.qq.com/mcp?key={}&format=0", api_key);

    println!("=== MCP Weather — QQ 地图 (HTTP) ===\n");

    let config = HttpConfig::new(&endpoint_url)
        .with_request_timeout(std::time::Duration::from_secs(60));
    let transport = HttpTransport::new(config);
    let client = McpClient::with_transport(transport).await;

    // 连接 + 初始化
    client.connect().await?;
    let result = client.initialize().await?;
    println!("✓ {} v{}", result.server_info.name, result.server_info.version);

    // 列出可用工具
    let list_resp = client
        .request(JsonRpcRequest::new(1, methods::TOOLS_LIST, None))
        .await?;

    let list_result: lellm_mcp::protocol::ListToolsResult =
        serde_json::from_value(match list_resp.result {
            lellm_mcp::protocol::JsonRpcResult::Success(v) => v,
            e => {
                println!("获取工具列表失败: {:?}", e);
                return Ok(());
            }
        })?;

    println!("✓ {} 个工具:", list_result.tools.len());
    for tool in &list_result.tools {
        println!("  - {}", tool.name);
    }

    // 查询天气
    let city = "陆家嘴";
    let tool_name = list_result
        .tools
        .iter()
        .find(|t| t.name.contains("weather"))
        .map(|t| t.name.as_str())
        .unwrap_or("weather");

    println!("\n查询 {} 天气 (工具: {})...\n", city, tool_name);

    let call_resp = client
        .request(JsonRpcRequest::new(
            2,
            methods::TOOLS_CALL,
            Some(serde_json::to_value(CallToolParams::new(
                tool_name,
                Some(serde_json::json!({ "address": city })),
            ))?),
        ))
        .await?;

    match call_resp.result {
        lellm_mcp::protocol::JsonRpcResult::Success(value) => {
            let call_result: lellm_mcp::protocol::CallToolResult =
                serde_json::from_value(value)?;
            for content in &call_result.content {
                if let Some(text) = content.as_text() {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
                        println!("{}", serde_json::to_string_pretty(&json)?);
                    } else {
                        println!("{}", text);
                    }
                }
            }
        }
        e => println!("请求失败: {:?}", e),
    }

    client.close().await?;
    Ok(())
}
