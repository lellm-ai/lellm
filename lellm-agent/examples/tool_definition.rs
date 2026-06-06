//! 工具定义 — 使用 derive(ToolDefinition) 宏
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//!
//! @tool
//! def search(query: str) -> str:
//!     """搜索信息。"""
//!     return f"结果：{query}"
//!
//! @tool
//! def get_weather(location: str) -> str:
//!     """获取位置的天气信息。"""
//!     return f"{location} 的天气：晴朗，72°F"
//! ```
//!
//! 运行：
//! ```text
//! cargo run --example tool_definition
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::{AgentBuilder, ToolArgs, ToolCategory, ToolRegistration, ToolUseLoop};
use lellm_core::{ChatResponse, ContentBlock, Message, TokenUsage, ToolDefinition};
use lellm_macros::ToolDefinition as ToolDefinitionDerive;
use lellm_provider::{MockProvider, ResolvedModel};
use std::sync::Arc;

// ─── 方式一：derive(ToolDefinition) 宏（推荐）────────────────────

/// 搜索工具参数 — 宏自动生成 JSON Schema
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "search", description = "搜索互联网信息")]
struct SearchArgs {
    /// 搜索关键词
    query: String,
}

/// 天气工具参数
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "get_weather", description = "获取指定位置的天气信息")]
struct WeatherArgs {
    /// 城市或地点名称
    location: String,
    /// 温度单位（摄氏度/华氏度），默认为摄氏度
    unit: Option<String>,
}

/// 使用 derive 宏注册工具
#[allow(dead_code)]
fn register_with_derive() -> Vec<ToolRegistration> {
    vec![
        // search 工具
        ToolRegistration::safe(SearchArgs::tool_definition(), |args| {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move { Ok(format!("搜索结果：{}", query)) }
        }),
        // get_weather 工具
        ToolRegistration::safe(WeatherArgs::tool_definition(), |args| {
            let location = args
                .get("location")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let unit = args
                .get("unit")
                .and_then(|v| v.as_str())
                .unwrap_or("摄氏度")
                .to_string();
            async move { Ok(format!("{} 的天气：晴朗，25{}", location, unit)) }
        }),
    ]
}

// ─── 方式二：手动构造 ToolDefinition ────────────────────────────

/// 手动构造工具定义 — 不依赖 derive 宏
#[allow(dead_code)]
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

// ─── 方式三：带安全分级的工具注册 ────────────────────────────────

/// 展示不同安全分级的工具注册方式
#[allow(dead_code)]
fn register_with_safety_levels() -> Vec<ToolRegistration> {
    let read_def = ToolDefinition {
        name: "read_file".to_string(),
        description: "读取文件内容".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        }),
    };

    let write_def = ToolDefinition {
        name: "write_file".to_string(),
        description: "写入文件内容".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        }),
    };

    vec![
        // Safe — 可并发执行
        ToolRegistration::safe(read_def, |args| {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move { Ok(format!("读取文件: {}", path)) }
        }),
        // CategoryExclusive — 同分类内互斥
        ToolRegistration::category_exclusive(write_def, ToolCategory::FILE_IO, |args| {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move { Ok(format!("写入文件: {}", path)) }
        }),
    ]
}

/// 构建带工具的 Agent
fn create_agent(tools: Vec<ToolRegistration>) -> ToolUseLoop {
    // MockProvider — 模拟 LLM 先调用工具，再返回最终答案
    let response = ChatResponse::new(
        vec![ContentBlock::text("已完成所有操作。".to_string())],
        TokenUsage::default(),
        serde_json::json!(null),
    );
    let provider = Arc::new(MockProvider::reply_with(response));

    let model = ResolvedModel {
        provider,
        model: "test-model".to_string(),
    };

    AgentBuilder::new(model).tools(tools).build()
}

#[tokio::main]
async fn main() {
    // ─── 展示 derive 宏生成的 Schema ───
    println!("=== SearchArgs Schema ===");
    let search_def = SearchArgs::tool_definition();
    println!("名称: {}", search_def.name);
    println!("描述: {}", search_def.description);
    println!(
        "Schema: {}",
        serde_json::to_string_pretty(&search_def.parameters).unwrap()
    );

    println!("\n=== WeatherArgs Schema ===");
    let weather_def = WeatherArgs::tool_definition();
    println!("名称: {}", weather_def.name);
    println!("描述: {}", weather_def.description);
    println!(
        "Schema: {}",
        serde_json::to_string_pretty(&weather_def.parameters).unwrap()
    );

    // ─── 构建并执行 Agent ───
    println!("\n=== 构建 Agent ===");
    let tools = register_with_derive();
    println!("注册了 {} 个工具", tools.len());
    for reg in &tools {
        println!(
            "  - {} ({})",
            reg.definition.name, reg.definition.description
        );
    }

    let agent = create_agent(tools);

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
