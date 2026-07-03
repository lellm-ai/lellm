//! MCP Weather Example — 使用 QQ 地图 MCP 服务器查询天气
//!
//! 功能：通过腾讯位置服务 MCP 服务器查询天气
//!
//! 前置条件：
//! 1. 在腾讯位置服务申请 API Key: https://lbs.qq.com/service/webService/webServiceGuide/overview
//! 2. 设置环境变量: export QQ_MAP_KEY=your_api_key
//!
//! 运行：
//! ```bash
//! QQ_MAP_KEY=your_api_key cargo run --example mcp_weather --features sse
//! ```

use lellm_mcp::client::McpClient;
use lellm_mcp::protocol::{CallToolParams, JsonRpcRequest, methods};
use lellm_mcp::transport::{SseConfig, SseTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_mcp=debug,info".into()),
        )
        .try_init();

    // 获取 API Key
    let api_key =
        std::env::var("QQ_MAP_KEY").expect("请设置环境变量 QQ_MAP_KEY (腾讯位置服务 API Key)");

    // 构建 SSE URL
    let sse_url = format!("https://mcp.map.qq.com/sse?key={}&format=0", api_key);

    println!("=== MCP Weather Example — QQ 地图 ===\n");
    println!("连接到: {}\n", sse_url);

    // 创建 SSE Transport
    let config = SseConfig::new(&sse_url).with_request_timeout(std::time::Duration::from_secs(60));
    let transport = SseTransport::new(config);

    // 创建 MCP Client
    let client = McpClient::with_transport(transport);

    // 连接
    println!("正在连接...");
    client.connect().await?;
    println!("✓ 连接成功\n");

    // 初始化
    println!("正在初始化...");
    let result = client.initialize().await?;
    println!("✓ 初始化成功");
    println!("  协议版本: {}", result.protocol_version);
    println!(
        "  服务器: {} v{}\n",
        result.server_info.name, result.server_info.version
    );

    // 列出可用工具
    println!("正在获取可用工具...");
    let list_req = JsonRpcRequest::new(1, methods::TOOLS_LIST, None);
    let list_resp = client.request(list_req).await?;

    let list_result: lellm_mcp::protocol::ListToolsResult =
        serde_json::from_value(match list_resp.result {
            lellm_mcp::protocol::JsonRpcResult::Success(v) => v,
            _ => return Err("获取工具列表失败".into()),
        })?;

    println!("✓ 发现 {} 个工具:\n", list_result.tools.len());
    for tool in &list_result.tools {
        println!("  - {}: {}", tool.name, tool.description);
    }
    println!();

    // 查询北京天气
    println!("=== 查询北京天气 ===\n");

    let city = "北京";

    println!("城市: {}\n", city);

    // 调用天气查询工具
    // QQ 地图 MCP 的天气工具名称可能是 weather 或 get_weather
    // 根据实际工具列表调整
    let tool_name = list_result
        .tools
        .iter()
        .find(|t| t.name.contains("weather"))
        .map(|t| t.name.clone())
        .unwrap_or_else(|| "weather".to_string());

    println!("使用工具: {}\n", tool_name);

    let call_params = CallToolParams::new(
        &tool_name,
        Some(serde_json::json!({
            "address": city
        })),
    );

    let call_req = JsonRpcRequest::new(
        2,
        methods::TOOLS_CALL,
        Some(serde_json::to_value(&call_params)?),
    );
    let call_resp = client.request(call_req).await?;

    match call_resp.result {
        lellm_mcp::protocol::JsonRpcResult::Success(value) => {
            let call_result: lellm_mcp::protocol::CallToolResult = serde_json::from_value(value)?;

            if call_result.is_error {
                println!("❌ 工具调用失败:");
                for content in &call_result.content {
                    if let Some(text) = content.as_text() {
                        println!("  {}", text);
                    }
                }
            } else {
                println!("✓ 天气查询成功:\n");
                for content in &call_result.content {
                    if let Some(text) = content.as_text() {
                        // 尝试格式化 JSON 输出
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(text) {
                            println!("{}", serde_json::to_string_pretty(&json)?);
                        } else {
                            println!("{}", text);
                        }
                    }
                }
            }
        }
        lellm_mcp::protocol::JsonRpcResult::Error(e) => {
            println!("❌ 请求错误: {}", e.message);
        }
    }

    // 关闭连接
    println!("\n正在关闭连接...");
    client.close().await?;
    println!("✓ 连接已关闭");

    Ok(())
}
