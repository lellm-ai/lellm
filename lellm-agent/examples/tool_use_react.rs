//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 天气查询链：LLM 推理城市拼音 → `fetch_weather` → 失败重试（真实 wttr.in API）
//!
//! ReAct 流程：
//! ```text
//! 用户: "帮我查一下浦东新区的天气"
//!   → LLM 推理: 浦东新区 → 上海 → shanghai
//!   → 调用: fetch_weather("shanghai")
//!   → ✅ 成功 → 自然语言总结
//!   → ❌ 失败 → LLM 重新推理 → 再次调用 → ...
//! ```
//!
//! 运行：
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example tool_use_react [地址]
//! ```

use lellm_agent::{
    AgentBuilder, AgentEvent, AgentStream, ToolArgs, ToolRegistration, ToolUseLoop,
    schemars::JsonSchema,
};
use lellm_core::{Message, ToolError, ToolErrorKind, text_block};
use lellm_macros::ToolDefinition;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use lellm_provider::{ProviderEvent, ResolvedModel};
use std::sync::Arc;

// ─── 工具定义 ───────────────────────────────────────────────────

#[derive(JsonSchema, ToolDefinition)]
#[tool(
    name = "fetch_weather",
    description = "获取全球任意城市的实时天气。\
                   返回 JSON: {{ city, condition, temperature, humidity, wind_speed }}。\
                   城市名使用英文（小写，多词用连字符），如 'shanghai', 'new-york', 'tokyo', 'london'。\
                   若返回 NotFound，请重新推理城市英文名后重试。"
)]
#[allow(dead_code)]
struct FetchWeatherArgs {
    /// 城市英文名，如 "shanghai"、"new york"、"tokyo"
    city: String,
}

/// 从 wttr.in 获取天气，直接返回 JSON 字符串
fn fetch_weather(city: &str) -> Result<String, ToolError> {
    let response = reqwest::blocking::get(format!("https://wttr.in/{city}?format=%c+%t+%h+%w"))
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Network,
            message: format!("请求 wttr.in 失败: {e}"),
        })?
        .text()
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("读取响应失败: {e}"),
        })?;

    let parts: Vec<&str> = response.split_whitespace().collect();
    if parts.is_empty() {
        return Err(ToolError {
            kind: ToolErrorKind::NotFound,
            message: format!(
                "城市 '{city}' 未找到，请检查英文名。\
                 示例: shanghai, new-york, tokyo, london, paris。\
                 请重新推理城市英文名（多词用连字符），再试。",
            ),
        });
    }

    Ok(serde_json::json!({
        "city": city,
        "condition": parts.first().unwrap_or(&""),
        "temperature": parts.get(1).unwrap_or(&""),
        "humidity": parts.get(2).unwrap_or(&""),
        "wind_speed": parts.get(3).unwrap_or(&""),
    })
    .to_string())
}

/// 创建天气查询工具注册
fn register_weather_tools() -> Vec<ToolRegistration> {
    vec![ToolRegistration::safe(
        FetchWeatherArgs::tool_definition(),
        |args: &serde_json::Value| {
            let city = args
                .get("city")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                tokio::task::spawn_blocking(move || fetch_weather(&city))
                    .await
                    .map_err(|e| ToolError {
                        kind: ToolErrorKind::Internal,
                        message: format!("任务执行失败: {e}"),
                    })?
            }
        },
    )]
}

// ─── Provider / Agent 工厂 ──────────────────────────────────────

fn create_provider() -> GenericProvider<OpenAICompatAdapter> {
    GenericProvider::new(
        OpenAICompatAdapter::openai(),
        ProviderConfig::bearer(
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            std::env::var("OPENAI_API_KEY").expect("请设置 OPENAI_API_KEY"),
        )
        .expect("Invalid base URL")
        .with_timeout(std::time::Duration::from_secs(
            std::env::var("OPENAI_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
        )),
    )
}

fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    AgentBuilder::new(ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    })
    .system_prompt(
        "你是一个天气查询助手。\
                    用户会告诉你一个地址，请推理对应的城市英文名，\
                    调用 fetch_weather(city) 获取天气。若返回 NotFound，\
                    请重新推理城市英文名后重试。获取到天气后，用自然语言总结。"
            .to_string(),
    )
    .tools(register_weather_tools())
    .max_iterations(10)
    .build()
}

// ─── ReAct 循环观测器 ───────────────────────────────────────────

/// 当前 ReAct 轮次的中间状态
#[derive(Debug, Default)]
struct RoundState {
    reasoning: String,
    current_tool: Option<String>,
}

async fn observe_react_loop(
    mut stream: AgentStream,
    question: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("================================ 人类消息 =================================");
    println!("{question}\n");

    let mut iteration: usize = 0;
    let mut round = RoundState::default();

    while let Some(event) = stream.recv().await {
        match event {
            // ─── Provider 事件 ───────────────────────────────────────
            AgentEvent::Provider(ProviderEvent::Start { model }) => {
                iteration += 1;
                round = RoundState::default();
                eprintln!("[DEBUG] >>> 第 {iteration} 轮 — 调用 {model}");
            }

            AgentEvent::Provider(ProviderEvent::Token { token }) => {
                round.reasoning.push_str(&token);
                print!("{token}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(ProviderEvent::ThinkingDelta { thinking, .. }) => {
                round.reasoning.push_str(&thinking);
                print!("[思考] {thinking}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(ProviderEvent::ResponseComplete { tool_calls, usage }) => {
                if tool_calls.is_empty() {
                    eprintln!("\n[DEBUG] >>> 第 {iteration} 轮 — 最终回答");
                    let _ = usage;
                } else {
                    println!();
                    println!(
                        "================================== AI 消息 =================================="
                    );
                    if !round.reasoning.is_empty() {
                        println!("推理: {}", round.reasoning);
                    }
                    println!("工具调用：");
                    for tc in &tool_calls {
                        println!("  {} ({})", tc.name, tc.id);
                        println!("  参数: {}", tc.arguments);
                    }
                    println!();
                    eprintln!(
                        "[DEBUG] >>> 第 {iteration} 轮 — {} 个工具调用",
                        tool_calls.len()
                    );
                }
            }

            // ─── 工具事件 ────────────────────────────────────────────
            AgentEvent::ToolStart { name, .. } => {
                round.current_tool = Some(name);
            }

            AgentEvent::ToolEnd { result, .. } => {
                println!(
                    "=============================== 工具观察 ================================"
                );
                match result {
                    Ok(output) => {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&output) {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&value).unwrap_or(output.clone())
                            );
                        } else {
                            println!("{output}");
                        }
                    }
                    Err(err) => {
                        println!("❌ 工具错误 [{}] {}", err.kind, err.message);
                    }
                }
                round.current_tool = None;
                println!();
            }

            AgentEvent::Retry {
                tool_call_id,
                attempt,
                max_attempts,
                reason,
            } => {
                println!(
                    "=============================== 工具观察 ================================"
                );
                println!("🔄 重试工具 {tool_call_id} (第 {attempt}/{max_attempts} 次): {reason}");
                println!();
            }

            // ─── 终态事件 ────────────────────────────────────────────
            AgentEvent::LoopEnd { result } => {
                println!();
                println!("--- 执行摘要 ---");
                println!("停止原因: {:?}", result.stop_reason);
                println!("迭代次数: {}", result.iterations);
                println!("工具调用总数: {}", result.tool_calls_executed);
                println!(
                    "Token 消耗: prompt={}, completion={}, total={}",
                    result.response.usage.prompt_tokens,
                    result.response.usage.completion_tokens,
                    result.response.usage.total_tokens,
                );
                return Ok(());
            }

            AgentEvent::LoopError { error, iterations } => {
                println!();
                println!("================================ 错误 =================================");
                println!("❌ Agent 执行失败（第 {iterations} 轮）: {error}");
                println!();
                return Err(format!("Agent 执行失败: {error}").into());
            }
        }
    }

    eprintln!("[WARN] Stream 意外结束，未收到终止事件");
    Ok(())
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!("错误：请设置 OPENAI_API_KEY 环境变量");
        eprintln!("用法：OPENAI_API_KEY=sk-xxx cargo run --example tool_use_react [地址]");
        std::process::exit(1);
    }

    let provider = create_provider();
    let agent = create_agent(provider);

    println!("=== LeLLM Agent — 天气查询链（真实 wttr.in API）===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下金桥的天气".to_string(),
    };

    let stream = agent.execute_stream(vec![Message::User {
        content: text_block(question.clone()),
    }]);
    observe_react_loop(stream, &question).await
}
