//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//! from langchain.agents import create_agent
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
//!
//! agent = create_agent(model, tools=[search, get_weather])
//! result = agent.invoke("找出当前最受欢迎的无线耳机并检查其库存")
//! ```
//!
//! 运行（需设置环境变量）：
//! ```text
//! OPENAI_BASE_URL=https://api.openai.com/v1 OPENAI_API_KEY=sk-xxx cargo run --example tool_use_real
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::{AgentBuilder, ToolArgs, ToolRegistration, ToolUseLoop};
use lellm_core::{ToolError, ToolErrorKind};
use lellm_macros::ToolDefinition as ToolDefinitionDerive;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use std::sync::Arc;

// ─── 定义工具 ───────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "search", description = "搜索互联网信息，返回搜索结果")]
struct SearchArgs {
    /// 搜索关键词
    query: String,
}

#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "get_weather", description = "获取指定位置的天气信息")]
struct GetWeatherArgs {
    /// 城市或地点名称
    location: String,
    /// 温度单位（摄氏度/华氏度），默认为摄氏度
    unit: Option<String>,
}

#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(
    name = "calculator",
    description = "执行数学计算，支持加减乘除等基本运算"
)]
struct CalculatorArgs {
    /// 数学表达式，例如 "2 + 3 * 4"
    expression: String,
}

// ─── 工具实现 ───────────────────────────────────────────────────

/// 注册所有工具
fn register_tools() -> Vec<ToolRegistration> {
    vec![
        // 搜索工具
        ToolRegistration::safe(SearchArgs::tool_definition(), |args| {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let results = match query.to_lowercase().as_str() {
                    q if q.contains("wireless") || q.contains("耳机") => {
                        format!(
                            "搜索结果：{}\n找到以下相关产品：\n1. Sony WH-1000XM5 - 降噪无线耳机，评分 4.8\n2. Apple AirPods Pro - 真无线降噪耳机，评分 4.7\n3. Bose QuietComfort 45 - 降噪头戴式耳机，评分 4.6\n4. Sennheiser Momentum 4 - 无线头戴式耳机，评分 4.5\n5. JBL Tune 760NC - 预算友好型降噪耳机，评分 4.3",
                            query
                        )
                    }
                    q if q.contains("rust") => {
                        format!(
                            "搜索结果：{}\nRust 是一门系统编程语言，专注于内存安全和并发性能。\n主要特点：零成本抽象、内存安全、并发无畏、模式匹配、类型推断",
                            query
                        )
                    }
                    _ => {
                        format!("搜索结果：{}\n找到 3 个相关结果，请查看详细信息。", query)
                    }
                };
                Ok(results)
            }
        }),
        // 天气工具
        ToolRegistration::safe(GetWeatherArgs::tool_definition(), |args| {
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
            async move {
                let weather = match location.to_lowercase().as_str() {
                    "北京" | "beijing" => (25, "晴", "45"),
                    "上海" | "shanghai" => (28, "多云", "60"),
                    "东京" | "tokyo" => (22, "小雨", "70"),
                    "纽约" | "new york" => (20, "晴", "40"),
                    "伦敦" | "london" => (15, "阴", "80"),
                    _ => (20, "晴", "50"),
                };
                let temp_display = match unit.as_str() {
                    u if u.contains("华氏") => {
                        format!("{}°F", (weather.0 as f64 * 9.0 / 5.0 + 32.0) as i32)
                    }
                    _ => format!("{}°C", weather.0),
                };
                Ok(format!(
                    "{} 的天气：{}，温度 {}，湿度 {}%",
                    location, weather.1, temp_display, weather.2
                ))
            }
        }),
        // 计算器工具
        ToolRegistration::safe(CalculatorArgs::tool_definition(), |args| {
            let expression = args
                .get("expression")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let result = evaluate_expression(&expression).map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("计算失败：{}", e),
                })?;
                Ok(format!("{} = {}", expression, result))
            }
        }),
    ]
}

/// 简单的表达式求值器 — 仅支持基本四则运算
fn evaluate_expression(expr: &str) -> Result<String, String> {
    let expr = expr.trim();
    let expr = expr.replace('×', "*").replace('÷', "/").replace('−', "-");

    // 安全检查：只允许数字和基本运算符
    for c in expr.chars() {
        if !c.is_digit(10) && !matches!(c, '+' | '-' | '*' | '/' | '(' | ')' | '.' | ' ') {
            return Err(format!("不支持的字符: {}", c));
        }
    }

    // 简单的分词与求值
    let tokens: Vec<&str> = expr.split_whitespace().collect();
    if tokens.len() < 3 {
        return Err("表达式格式错误，期望: 数字 运算符 数字".to_string());
    }

    let left: f64 = tokens[0]
        .parse()
        .map_err(|_| format!("无效数字: {}", tokens[0]))?;
    let op = tokens[1];
    let right: f64 = tokens[2]
        .parse()
        .map_err(|_| format!("无效数字: {}", tokens[2]))?;

    let result = match op {
        "+" => left + right,
        "-" => left - right,
        "*" => left * right,
        "/" => {
            if right == 0.0 {
                return Err("除以零".to_string());
            }
            left / right
        }
        _ => return Err(format!("不支持的运算符: {}", op)),
    };

    // 如果是整数，不显示小数点
    if result.fract() == 0.0 {
        Ok(format!("{:.0}", result))
    } else {
        Ok(format!("{:.2}", result))
    }
}

// ─── 创建 Agent ─────────────────────────────────────────────────

/// 从环境变量创建真实 Provider
fn create_provider() -> GenericProvider<OpenAICompatAdapter> {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let api_key = std::env::var("OPENAI_API_KEY").expect("请设置 OPENAI_API_KEY 环境变量");
    let timeout = std::env::var("OPENAI_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);

    GenericProvider::new(
        OpenAICompatAdapter::openai(),
        ProviderConfig::bearer(&base_url, api_key)
            .expect("Invalid base URL")
            .with_timeout(std::time::Duration::from_secs(timeout)),
    )
}

/// 创建带工具的 Agent
fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    };

    // 注册工具
    let tools = register_tools();

    AgentBuilder::new(model)
        .system_prompt(
            "你是一个有帮助的助手。你可以使用搜索、天气查询和计算器工具来帮助用户。请根据用户的问题选择合适的工具。"
                .to_string(),
        )
        .tools(tools)
        .max_iterations(10)
        .build()
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 检查环境变量
    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!("错误：请设置 OPENAI_API_KEY 环境变量");
        eprintln!("用法：OPENAI_API_KEY=sk-xxx cargo run --example tool_use_real");
        std::process::exit(1);
    }

    let provider = create_provider();
    let agent = create_agent(provider);

    println!("=== LeLLM Agent — 真实 Provider 工具调用示例 ===\n");

    // 读取用户输入（从命令行参数或默认问题）
    let question = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "搜索一下 Rust 编程语言，然后告诉我它的主要特点。".to_string());

    println!("用户问题：{}\n", question);
    println!("--- 开始执行 ---\n");

    let result = agent
        .execute(vec![lellm_core::Message::User {
            content: lellm_core::text_block(question),
        }])
        .await
        .map_err(|e| format!("Agent 执行失败：{}", e))?;

    // 打印对话历史
    println!("--- 对话历史 ---\n");
    for (_i, msg) in result.messages.iter().enumerate() {
        match msg {
            lellm_core::Message::System { content } => {
                println!("[系统]");
                for block in content {
                    if let lellm_core::ContentBlock::Text(t) = block {
                        println!("  {}", t.text);
                    }
                }
                println!();
            }
            lellm_core::Message::User { content } => {
                println!("[用户]");
                for block in content {
                    if let lellm_core::ContentBlock::Text(t) = block {
                        println!("  {}", t.text);
                    }
                }
                println!();
            }
            lellm_core::Message::Assistant { content } => {
                println!("[AI]");
                for block in content {
                    match block {
                        lellm_core::ContentBlock::Text(t) => {
                            println!("  文本: {}", t.text);
                        }
                        lellm_core::ContentBlock::ToolCall(tc) => {
                            println!("  工具调用: {}({})", tc.name, tc.arguments);
                        }
                        _ => {}
                    }
                }
                println!();
            }
            lellm_core::Message::ToolResult {
                tool_call_id,
                is_error,
                content,
            } => {
                let status = if *is_error {
                    "❌ 错误"
                } else {
                    "✅ 结果"
                };
                println!("[工具 {}] tool_call_id={}", status, tool_call_id);
                for block in content {
                    if let lellm_core::ContentBlock::Text(t) = block {
                        // 截断长文本
                        let text = if t.text.len() > 200 {
                            format!("{}...", &t.text[..200])
                        } else {
                            t.text.clone()
                        };
                        println!("  {}", text);
                    }
                }
                println!();
            }
        }
    }

    // 打印最终回复
    println!("--- 最终回复 ---\n");
    for block in &result.response.content {
        if let lellm_core::ContentBlock::Text(t) = block {
            println!("{}", t.text);
        }
    }

    // 打印执行摘要
    println!("\n--- 执行摘要 ---");
    println!("停止原因: {:?}", result.stop_reason);
    println!("迭代次数: {}", result.iterations);
    println!("工具调用总数: {}", result.tool_calls_executed);
    println!(
        "Token 消耗: prompt={}, completion={}, total={}",
        result.response.usage.prompt_tokens,
        result.response.usage.completion_tokens,
        result.response.usage.total_tokens,
    );

    Ok(())
}
