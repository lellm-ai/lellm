//! 三级 Tool API 展示
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//!
//! @tool
//! def search(query: str) -> str:
//!     """搜索信息。"""
//!     return f"结果：{query}"
//! ```
//!
//! 运行：
//! ```text
//! cargo run --example tool_definition
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::serde::Deserialize;
use lellm_agent::{
    AgentBuilder, ToolArgs, ToolCategory, ToolRegistration, ToolResult, ToolUseLoop,
};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage, ToolDefinition};
use lellm_macros::{tool, Tool};
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

// ─── Level 1: #[tool] 函数宏（推荐，95% 用户）──────────────────

/// 搜索互联网信息
#[tool(name = "search", description = "搜索互联网信息")]
fn search(query: String, limit: Option<u32>) -> ToolResult {
    Ok(format!("搜索结果: {} (限制: {:?})", query, limit))
}

/// 获取指定位置的天气信息
#[tool(name = "get_weather", description = "获取指定位置的天气信息")]
fn get_weather(location: String, unit: Option<String>) -> ToolResult {
    let unit = unit.unwrap_or_else(|| "摄氏度".to_string());
    Ok(format!("{} 的天气：晴朗，25{}", location, unit))
}

// ─── Level 2: #[derive(Tool)] + safe()（高级用户）─────────────

/// 天气工具参数
#[derive(Deserialize, JsonSchema, Tool)]
#[tool(name = "weather_search", description = "搜索天气信息")]
struct WeatherArgs {
    /// 城市名称
    city: String,
    /// 单位（摄氏度/华氏度）
    unit: Option<String>,
    /// 是否包含预报
    include_forecast: bool,
}

// ─── Level 3: ToolRegistration::safe()（框架开发者）────────────

/// 手动构造工具定义 — 不依赖宏
fn register_manually() -> Vec<ToolRegistration> {
    let search_def = ToolDefinition {
        name: "manual_search".to_string(),
        description: "手动定义的搜索工具".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "搜索关键词" }
            },
            "required": ["query"]
        }),
    };

    vec![ToolRegistration::safe(search_def, |args| {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move { Ok(format!("手动搜索结果：{}", query)) }
    })]
}

/// 使用 Level 1 注册工具
fn register_level1() -> Vec<ToolRegistration> {
    vec![search_tool(), get_weather_tool()]
}

/// 使用 Level 2 注册工具
fn register_level2() -> Vec<ToolRegistration> {
    vec![WeatherArgs::safe(|args| async move {
        let forecast = if args.include_forecast { "含预报" } else { "无预报" };
        Ok(format!(
            "{} 天气: 晴朗, 25{}, {}",
            args.city,
            args.unit.unwrap_or_else(|| "摄氏度".to_string()),
            forecast
        ))
    })]
}

/// 构建带工具的 Agent
fn create_agent(tools: Vec<ToolRegistration>) -> ToolUseLoop {
    let response = ChatResponse::new(
        vec![ContentBlock::text("已完成所有操作。".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        context_window: None,
        provider,
        model: "test-model".to_string(),
    };

    AgentBuilder::new(model).tools(tools).build()
}

#[tokio::main]
async fn main() {
    // ─── Level 1: #[tool] 函数宏 ───
    println!("=== Level 1: #[tool] 函数宏 ===");
    println!("搜索工具: {} - {}", SearchArgs::NAME, SearchArgs::DESCRIPTION);
    println!(
        "Schema: {}",
        serde_json::to_string_pretty(&SearchArgs::__schema()).unwrap()
    );
    println!();

    println!("天气工具: {} - {}", GetWeatherArgs::NAME, GetWeatherArgs::DESCRIPTION);
    println!();

    // ─── Level 2: #[derive(Tool)] + safe() ───
    println!("=== Level 2: #[derive(Tool)] + safe() ===");
    println!(
        "天气搜索: {} - {}",
        WeatherArgs::NAME,
        WeatherArgs::DESCRIPTION
    );
    println!(
        "Schema: {}",
        serde_json::to_string_pretty(&WeatherArgs::__schema()).unwrap()
    );
    println!();

    // ─── Level 3: ToolRegistration ───
    println!("=== Level 3: ToolRegistration::safe() ===");
    let manual = register_manually();
    for reg in &manual {
        println!("  - {} ({})", reg.definition.name, reg.definition.description);
    }
    println!();

    // ─── 验证安全分级 ───
    println!("=== 安全分级 ===");
    let l1 = register_level1();
    println!("Level 1 工具数量: {}", l1.len());
    for reg in &l1 {
        println!("  - {} (safety: {:?})", reg.definition.name, reg.safety);
    }

    let l2 = register_level2();
    println!("Level 2 工具数量: {}", l2.len());
    for reg in &l2 {
        println!("  - {} (safety: {:?})", reg.definition.name, reg.safety);
    }

    // 验证 category_exclusive
    let cat_exclusive = WeatherArgs::category_exclusive(ToolCategory::NETWORK, |args| async move {
        Ok(format!("网络请求: {}", args.city))
    });
    println!(
        "CategoryExclusive 工具: {} (safety: {:?})",
        cat_exclusive.definition.name, cat_exclusive.safety
    );

    // 验证 exclusive
    let exclusive = WeatherArgs::exclusive(|args| async move {
        Ok(format!("独占执行: {}", args.city))
    });
    println!(
        "Exclusive 工具: {} (safety: {:?})",
        exclusive.definition.name, exclusive.safety
    );

    // ─── 构建并执行 Agent ───
    println!("\n=== 构建 Agent ===");
    let all_tools = [register_level1(), register_level2(), register_manually()].concat();
    println!("注册了 {} 个工具", all_tools.len());

    let agent = create_agent(all_tools);

    println!("\n=== 执行 Agent ===");
    let result = agent
        .execute(vec![Message::User {
            content: lellm_core::text_block("搜索一下 Rust 编程语言。".to_string()),
        }])
        .await
        .expect("Agent 执行失败");

    println!("迭代次数: {}", result.iterations);
    println!("停止原因: {:?}", result.stop_reason);
}
