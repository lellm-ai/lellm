//! MCP Server Example — 使用 SimpleMcp 创建自定义 MCP 服务器
//!
//! 演示：
//! - 定义工具（add、multiply）
//! - 以 stdio 模式运行
//!
//! 运行：
//! ```bash
//! cargo run --example mcp_server --features server -p lellm-mcp
//! ```

use lellm_mcp::SimpleMcp;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut mcp = SimpleMcp::new("Math Server");

    // 注册 add 工具
    mcp.tool("add", "Add two numbers", |args| async move {
        let a = args["a"].as_i64().unwrap_or(0);
        let b = args["b"].as_i64().unwrap_or(0);
        Ok(serde_json::json!({ "result": a + b }))
    });

    // 注册 multiply 工具
    mcp.tool("multiply", "Multiply two numbers", |args| async move {
        let a = args["a"].as_i64().unwrap_or(0);
        let b = args["b"].as_i64().unwrap_or(0);
        Ok(serde_json::json!({ "result": a * b }))
    });

    println!("=== MCP Server (stdio) ===");
    println!("等待 JSON-RPC 请求...\n");

    // 以 stdio 模式运行
    mcp.run_stdio().await?;

    Ok(())
}
