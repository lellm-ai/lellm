//! MCP Multi-Server Example — 同时连接多个 MCP 服务器
//!
//! 演示 McpServerRegistry 用法：
//! - 连接多个 MCP 服务器（SSE/HTTP）
//! - 合并工具列表
//! - 工具调用自动路由到对应服务器
//!
//! 运行：
//! ```bash
//! TENCENT_MAP_KEY=your_api_key cargo run --example mcp_multi --features bridge,sse,http -p lellm-mcp
//! ```

use lellm_mcp::{McpServerRegistry, ToolCatalog};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("TENCENT_MAP_KEY").expect("请设置环境变量 TENCENT_MAP_KEY");

    println!("=== MCP Multi-Server Example ===\n");

    let mut registry = McpServerRegistry::new();

    // 添加多个服务器（这里用同一个服务器演示，实际可以连接不同服务器）
    let sse_url = format!("https://mcp.map.qq.com/sse?key={}&format=0", api_key);
    registry.add_sse("qq-map-sse", &sse_url).await?;

    let http_url = format!("https://mcp.map.qq.com/mcp?key={}&format=0", api_key);
    registry.add_http("qq-map-http", &http_url).await?;

    // 显示工具列表
    println!("已连接服务器:");
    for (server_name, tools) in registry.tool_names() {
        println!("  {} ({} 个工具)", server_name, tools.len());
        for tool in &tools {
            println!("    - {}", tool);
        }
    }
    println!("\n共 {} 个工具\n", registry.total_tools());

    // 获取 ToolCatalog 快照，直接调用工具
    let snapshot = registry.snapshot().await;

    // 调用 geocoder 工具（通过 snapshot 中的 ExecutableTool）
    let addresses = vec!["陆家嘴", "天安门", "奇台"];

    for addr in &addresses {
        println!("解析: {}", addr);
        if let Some(tool) = snapshot.get("geocoder") {
            match tool.execute(&serde_json::json!({ "address": addr })).await {
                Ok(result) => {
                    println!("  {}\n", result);
                }
                Err(e) => {
                    println!("  失败: {}\n", e);
                }
            }
        } else {
            println!("  未找到 geocoder 工具\n");
        }
    }

    Ok(())
}
