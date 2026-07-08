//! weather_agent_mcp — 天气查询链（MCP 版本）
//!
//! 工具链：`MCP resolve_city(腾讯地图) → http_get(wttr.in) → LLM 解析为 JSON`
//!
//! 通过 MCP 连接调用本地 Tencent Map Server 获取城市信息，
//! 替代原有的内嵌 resolve_city 四级降级逻辑。
//!
//! 支持两种传输模式：
//! - HTTP (默认): MCP_SERVER_URL=http://localhost:3100/mcp
//! - SSE: MCP_SERVER_URL=http://localhost:3100/sse
//!
//! ```text
//! # 终端 1: 启动 MCP Server (HTTP 模式)
//! TENCENT_MAP_KEY=xxx cargo run --example mcp_tencent_map_server --features server -p lellm-mcp
//!
//! # 终端 2: 运行 Weather Agent
//! OPENAI_API_KEY=sk-xxx cargo run --example weather_agent_mcp [地址]
//!
//! # 或使用 SSE 模式
//! MCP_TRANSPORT=sse cargo run --example weather_agent_mcp [地址]
//! ```

#[path = "_shared/shared.rs"]
mod shared;

use lellm_agent::AgentBuilder;
use lellm_agent::McpCatalog;
use lellm_core::{Message, ToolError, ToolErrorKind, ToolResult, text_block};
use lellm_derive::tool;
use lellm_mcp::McpClient;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::CodecProvider;
use lellm_provider::providers::openai_compat::OpenAICompatCodec;
use std::sync::Arc;

// ─── Tool: http_get ─────────────────────────────────────────────

/// 发送 HTTP GET 请求并返回响应文本。URL 由你根据 API 文档构造。
#[tool(
    name = "http_get",
    description = "发送 HTTP GET 请求并返回响应文本。URL 由你根据 API 文档构造。"
)]
async fn http_get(url: String) -> ToolResult {
    let body = tokio::task::spawn_blocking(move || {
        reqwest::blocking::get(&url)
            .map_err(|e| ToolError {
                kind: ToolErrorKind::Network,
                message: format!("请求失败: {e}"),
            })?
            .text()
            .map_err(|e| ToolError {
                kind: ToolErrorKind::Internal,
                message: format!("读取响应失败: {e}"),
            })
    })
    .await
    .map_err(|e| ToolError {
        kind: ToolErrorKind::Internal,
        message: format!("任务失败: {e}"),
    })??;
    Ok(serde_json::json!(body))
}

// ─── 分层 System Prompt — 最大化前缀缓存 ────────────────────────

/// 构建分层 Prompt，全部 cached — 用户查询通过 user message 传递。
fn build_system_prompt() -> lellm_core::Prompt {
    lellm_core::Prompt::new()
        // L1 — 核心身份
        .stable("你是天气查询助手。")
        // L2 — 工具使用指南
        .stable(
            "流程：
1. 提取用户输入中的所有地址
2. 对每个地址调用 resolve_city（MCP 工具，调用腾讯地图 API）
3. 对 city_en != \"unknown\" 调用 http_get(https://wttr.in/{city_en}?format=%c+%t+%h+%w)
4. 解析 wttr.in 返回的文本，提取天气数据，输出 JSON",
        )
        // L3 — 字段转换规则
        .stable(
            "wttr.in 返回格式: \"🌧️ +17°C 94% ↖11km/h\"
你需要转换以下字段：

1. condition（emoji → 中文）：
   - 🌧️/🌦️/🌧 → 小雨/中雨/大雨
   - ☀️/🌤 → 晴/多云
   - 其他 emoji 自行翻译为对应的中文天气描述

2. temperature（格式修正）：
   - \"+23°C\" → \"23°C\"（去掉 + 号）
   - \"-5°C\" → \"零下5°C\"（负数加\"零下\"）

3. wind（方向箭头 → 中文）：
   - \"→\" → \"东风\", \"←\" → \"西风\", \"↑\" → \"南风\", \"↓\" → \"北风\"
   - \"↗\" → \"东南风\", \"↘\" → \"西南风\", \"↙\" → \"西北风\", \"↖\" → \"东北风\"
   - \"↖11km/h\" → \"东北风11km/h\"
   - 无箭头（如 \"7km/h\"）→ 保持原样",
        )
        // L4 — 输出格式规则
        .stable(
            "输出格式（纯紧凑JSON，禁止任何解释文字）：
单地址: {\"city\":\"tokyo\",\"address\":\"新宿\",\"condition\":\"小雨\",\"temperature\":\"17°C\",\"humidity\":\"94%\",\"wind\":\"东风7km/h\"}
多地址: [{...},{...}]

最终回答必须为纯 JSON，不要包含 markdown 代码块标记或任何解释",
        )
}

// ─── MCP Server 连接 ───────────────────────────────────────────

/// 连接本地 Tencent Map MCP Server，返回 McpCatalog。
///
/// 根据 transport_type 选择传输模式：
/// - "sse": 使用 SSE Transport（GET /sse + POST /messages）
/// - 其他: 使用 HTTP Transport（POST /mcp）
async fn connect_tencent_map_server(
    server_url: &str,
    transport_type: &str,
) -> Result<Arc<dyn lellm_agent::ToolCatalog>, Box<dyn std::error::Error>> {
    let client = match transport_type {
        "sse" => {
            println!("Using SSE Transport");
            McpClient::connect_sse(server_url).await?
        }
        _ => {
            println!("Using HTTP Transport");
            McpClient::connect_http(server_url).await?
        }
    };

    // McpCatalog::from_client 内部会调用 tools/list 并缓存工具定义
    let catalog = McpCatalog::from_client(Arc::new(client)).await?;
    println!("✓ MCP Tools: {}", catalog.len());

    if catalog.is_empty() {
        return Err("MCP Server 未返回任何工具".into());
    }

    Ok(Arc::new(catalog))
}

// ─── Agent 工厂 ─────────────────────────────────────────────────

async fn create_agent(
    provider: CodecProvider<OpenAICompatCodec>,
    mcp_server_url: &str,
    transport_type: &str,
) -> Result<lellm_agent::ToolUseLoop, Box<dyn std::error::Error>> {
    // 连接 MCP Server
    let mcp_catalog = connect_tencent_map_server(mcp_server_url, transport_type).await?;

    Ok(AgentBuilder::new(ResolvedModel {
        provider: Arc::new(provider),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system(build_system_prompt())
    // 本地工具
    .tool(http_get_tool())
    // MCP 动态工具（resolve_city）
    .catalog(mcp_catalog)
    .max_iterations(10)
    .max_output_tokens(8000)
    .compile())
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=trace,lellm_provider=trace,info".into()),
        )
        .try_init();

    // MCP 传输模式: "sse" 或 "http" (默认)
    let transport_type = std::env::var("MCP_TRANSPORT")
        .ok()
        .unwrap_or_else(|| "http".to_string());

    // MCP Server 地址（可通过环境变量覆盖）
    let mcp_server_url = match transport_type.as_str() {
        "sse" => std::env::var("MCP_SERVER_URL")
            .ok()
            .unwrap_or_else(|| "http://localhost:3100/sse".to_string()),
        _ => std::env::var("MCP_SERVER_URL")
            .ok()
            .unwrap_or_else(|| "http://localhost:3100/mcp".to_string()),
    };

    println!("=== Weather Agent — MCP 版本 ===");
    println!("MCP Server: {}", mcp_server_url);
    println!("Transport: {}", transport_type);
    println!();

    let provider =
        CodecProvider::load(OpenAICompatCodec::llama()).expect("LLaMA provider env error");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下陆家嘴/新宿/阿尔卡吉/奇台的天气".to_string(),
    };

    let agent = create_agent(provider, &mcp_server_url, &transport_type).await?;

    let stream = agent.invoke_stream(vec![Message::user(text_block(question.clone()))]);
    shared::observe_react_loop(stream, &question).await
}
