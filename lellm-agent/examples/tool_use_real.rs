//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 包含两个示例场景：
//! 1. **产品搜索链**：`search_products` → `check_inventory`（模拟数据）
//! 2. **天气查询链**：LLM 推理城市拼音 → `fetch_weather`（真实 wttr.in API）
//!
//! 天气查询链中，**城市识别完全由大模型推理完成**：
//! - 用户输入任意地址（街道、乡镇、区、县）
//! - LLM 自行推理出所属地级市，并转换为拼音
//! - LLM 调用 `fetch_weather(city_pinyin)` 获取天气
//! - 无需任何硬编码映射表
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//! from langchain.agents import create_agent
//!
//! @tool
//! def fetch_weather(city_pinyin: str) -> str:
//!     """调用 wttr.in 获取城市天气。城市名用拼音，如 'shanghai', 'beijing'。"""
//!     return curl("-s", f"wttr.in/{city_pinyin}?format=%c+%t+%h+%w")
//!
//! agent = create_agent(model, tools=[fetch_weather])
//! result = agent.invoke("帮我查一下浦东新区的天气")
//! ```
//!
//! 智能体遵循 ReAct（推理 + 行动）模式，在推理步骤与工具调用之间交替，
//! 并将结果观察反馈到后续决策中，直到能够提供最终答案。
//!
//! 每一步都清晰可观测：
//! - 人类消息 → AI 消息（工具调用） → 工具观察 → AI 消息（工具调用） → ... → 最终答案
//! - 工具执行错误会以 "工具错误" 形式展示，不中断循环
//! - Provider API 错误会以 "API 错误" 形式展示
//!
//! 运行（需设置环境变量）：
//! ```text
//! OPENAI_BASE_URL=https://api.openai.com/v1 OPENAI_API_KEY=sk-xxx cargo run --example tool_use_real
//! ```

use lellm_agent::schemars::JsonSchema;
use lellm_agent::{AgentBuilder, AgentEvent, ToolArgs, ToolRegistration, ToolUseLoop};
use lellm_core::{ToolError, ToolErrorKind};
use lellm_macros::ToolDefinition as ToolDefinitionDerive;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::{GenericProvider, ProviderConfig};
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use std::sync::Arc;

// ─── 工具定义 ───────────────────────────────────────────────────

/// 获取城市天气
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(
    name = "fetch_weather",
    description = "调用 wttr.in API 获取指定城市的实时天气情况。\
                   返回 JSON 结构化数据，包含天气状况、温度、湿度、风速。\
                   城市名称必须使用拼音，例如 'shanghai'、'beijing'、'guangzhou'、'shenzhen'。"
)]
struct FetchWeatherArgs {
    /// 城市拼音名称，例如 "shanghai"、"beijing"、"guangzhou"
    city_pinyin: String,
}

/// 搜索产品
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(
    name = "search_products",
    description = "搜索产品目录，返回匹配的产品列表"
)]
struct SearchProductsArgs {
    /// 搜索关键词
    query: String,
}

/// 检查库存
#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "check_inventory", description = "检查指定产品的库存数量")]
struct CheckInventoryArgs {
    /// 产品 ID
    product_id: String,
}

// ─── 天气 API 响应结构 ──────────────────────────────────────────

/// wttr.in 天气返回的结构化数据
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct WeatherInfo {
    /// 城市名称（拼音）
    city: String,
    /// 天气状况，如 "小雨"、"晴"、"多云"
    condition: String,
    /// 温度，如 "17°C"
    temperature: String,
    /// 湿度，如 "94%"
    humidity: String,
    /// 风速，如 "7km/h"
    wind_speed: String,
}

/// 调用 wttr.in API 获取天气（blocking，应在 spawn_blocking 中调用）
fn fetch_weather_from_wttr(city_pinyin: &str) -> Result<WeatherInfo, ToolError> {
    let url = format!(
        "https://wttr.in/{}?format=%c+%t+%h+%w",
        city_pinyin
    );

    let response = reqwest::blocking::get(url)
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Network,
            message: format!("请求 wttr.in 失败: {}", e),
        })?
        .text()
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("读取响应失败: {}", e),
        })?;

    let parts: Vec<&str> = response.split_whitespace().collect();

    if parts.is_empty() {
        return Err(ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("wttr.in 返回空数据，城市 '{}' 可能不存在", city_pinyin),
        });
    }

    Ok(WeatherInfo {
        city: city_pinyin.to_string(),
        condition: parts.first().unwrap_or(&"").to_string(),
        temperature: parts.get(1).unwrap_or(&"").to_string(),
        humidity: parts.get(2).unwrap_or(&"").to_string(),
        wind_speed: parts.get(3).unwrap_or(&"").to_string(),
    })
}

// ─── 工具注册 ───────────────────────────────────────────────────

/// 注册天气查询工具（真实 wttr.in API）
fn register_weather_tools() -> Vec<ToolRegistration> {
    vec![ToolRegistration::safe(
        FetchWeatherArgs::tool_definition(),
        |args| {
            let city_pinyin = args
                .get("city_pinyin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let join_result =
                    tokio::task::spawn_blocking(move || fetch_weather_from_wttr(&city_pinyin))
                        .await;
                let weather = join_result.map_err(|e| ToolError {
                    kind: ToolErrorKind::Internal,
                    message: format!("任务执行失败: {}", e),
                })??;

                Ok(serde_json::json!({
                    "city": weather.city,
                    "condition": weather.condition,
                    "temperature": weather.temperature,
                    "humidity": weather.humidity,
                    "wind_speed": weather.wind_speed
                })
                .to_string())
            }
        },
    )]
}

/// 注册产品搜索链工具（模拟数据）
fn register_product_tools() -> Vec<ToolRegistration> {
    vec![
        ToolRegistration::safe(SearchProductsArgs::tool_definition(), |args| {
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let results = match query.to_lowercase().as_str() {
                    q if q.contains("wireless") || q.contains("耳机") => {
                        format!(
                            "找到 5 个匹配\"{}\"的产品：\n1. Sony WH-1000XM5 - 降噪无线耳机，评分 4.8\n2. Apple AirPods Pro - 真无线降噪耳机，评分 4.7\n3. Bose QuietComfort 45 - 降噪头戴式耳机，评分 4.6\n4. Sennheiser Momentum 4 - 无线头戴式耳机，评分 4.5\n5. JBL Tune 760NC - 预算友好型降噪耳机，评分 4.3",
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
        ToolRegistration::safe(CheckInventoryArgs::tool_definition(), |args| {
            let product_id = args
                .get("product_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                match product_id.to_lowercase().as_str() {
                    pid if pid.contains("wh-1000xm5") => {
                        Ok("产品 WH-1000XM5：库存 10 件，预计明日发货".to_string())
                    }
                    pid if pid.contains("airpods") => {
                        Ok("产品 AirPods Pro：库存 25 件，预计今日发货".to_string())
                    }
                    _ => Err(ToolError {
                        kind: ToolErrorKind::NotFound,
                        message: format!("产品 {} 未找到。", product_id),
                    }),
                }
            }
        }),
    ]
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

/// 创建天气查询 Agent — LLM 自行推理城市拼音
fn create_weather_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    };

    AgentBuilder::new(model)
        .system_prompt(
            "你是一个天气查询助手。\
             用户会告诉你一个地址（可能是街道、乡镇、区县等），\
             你需要先推理出该地址所属的地级市名称，并将其转换为拼音，\
             然后调用 fetch_weather 工具获取该城市的天气信息。\
             最后用自然语言总结天气情况。"
                .to_string(),
        )
        .tools(register_weather_tools())
        .max_iterations(10)
        .build()
}

/// 创建产品搜索 Agent（模拟数据链）
fn create_product_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    };

    AgentBuilder::new(model)
        .system_prompt(
            "你是一个有帮助的助手。你可以使用搜索产品、检查库存工具来帮助用户。\
             在回答前，请先使用工具获取所需信息，再给出最终答案。"
                .to_string(),
        )
        .tools(register_product_tools())
        .max_iterations(10)
        .build()
}

// ─── ReAct 循环观测器 ───────────────────────────────────────────

/// 当前 ReAct 轮次的中间状态。
#[derive(Debug, Default)]
struct RoundState {
    /// 本轮 LLM 输出的推理文本（Token 累积）
    reasoning: String,
    /// 已收集的工具结果（按执行顺序）
    tool_observations: Vec<(String, Result<String, lellm_core::ToolError>)>,
    /// 当前正在执行的工具名称
    current_tool_name: Option<String>,
}

/// 以 LangChain ReAct 格式实时观测 Agent 执行过程。
///
/// 事件流顺序（由 `execute_stream` 保证）：
/// ```text
/// Provider(Start) → Provider(Token)* → Provider(ResponseComplete{tool_calls})
///   → [ToolStart → ToolEnd] * N  (N = tool_calls.len())
///   → Provider(Start) → ... (下一轮)
/// → LoopEnd | LoopError
/// ```
async fn observe_react_loop(
    mut stream: lellm_agent::AgentStream,
    question: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("================================ 人类消息 =================================");
    println!("{}", question);
    println!();

    let mut iteration: usize = 0;
    let mut round = RoundState::default();

    while let Some(event) = stream.recv().await {
        match event {
            // ─── Provider 事件 ───────────────────────────────────────
            AgentEvent::Provider(lellm_provider::ProviderEvent::Start { model }) => {
                iteration += 1;
                round = RoundState::default();
                eprintln!("[DEBUG] >>> 第 {} 轮 — 调用 {}", iteration, model);
            }

            AgentEvent::Provider(lellm_provider::ProviderEvent::Token { token }) => {
                round.reasoning.push_str(&token);
                print!("{}", token);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(
                lellm_provider::ProviderEvent::ThinkingDelta { thinking, .. },
            ) => {
                round.reasoning.push_str(&thinking);
                print!("[思考] {}", thinking);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(
                lellm_provider::ProviderEvent::ResponseComplete {
                    tool_calls,
                    usage,
                },
            ) => {
                if tool_calls.is_empty() {
                    eprintln!("\n[DEBUG] >>> 第 {} 轮 — 最终回答", iteration);
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
                        "[DEBUG] >>> 第 {} 轮 — {} 个工具调用",
                        iteration,
                        tool_calls.len()
                    );
                }
            }

            // ─── 工具事件 ────────────────────────────────────────────
            AgentEvent::ToolStart { name, .. } => {
                round.current_tool_name = Some(name);
            }

            AgentEvent::ToolEnd { result, .. } => {
                let tool_name = round
                    .current_tool_name
                    .take()
                    .unwrap_or_else(|| "unknown".to_string());

                round.tool_observations.push((tool_name, result.clone()));

                println!(
                    "=============================== 工具观察 ================================"
                );
                match &round.tool_observations.last().unwrap().1 {
                    Ok(output) => {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(output) {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&value).unwrap_or(output.clone())
                            );
                        } else {
                            println!("{}", output);
                        }
                    }
                    Err(err) => {
                        println!("❌ 工具错误 [{}] {}", err.kind, err.message);
                    }
                }
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
                println!(
                    "🔄 重试工具 {} (第 {}/{} 次): {}",
                    tool_call_id, attempt, max_attempts, reason
                );
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
                println!("❌ Agent 执行失败（第 {} 轮）: {}", iterations, error);
                println!();
                return Err(format!("Agent 执行失败: {}", error).into());
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
        eprintln!("用法：OPENAI_API_KEY=sk-xxx cargo run --example tool_use_real");
        std::process::exit(1);
    }

    let provider = create_provider();

    // 命令行参数选择场景：
    //   - 无参数 或 "weather" → 天气查询链（真实 wttr.in API）
    //   - "product" → 产品搜索链（模拟数据）
    //   - 其他文本 → 天气查询链，文本作为地址输入
    let scenario = std::env::args().nth(1);
    match scenario {
        Some(arg) if arg == "product" => {
            let agent = create_product_agent(provider);
            println!("=== LeLLM Agent — 产品搜索链（模拟数据）===\n");
            let question = "找出当前最受欢迎的无线耳机并检查其库存";
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block(question.to_string()),
            }]);
            observe_react_loop(stream, question).await
        }
        Some(address) if address != "weather" => {
            let agent = create_weather_agent(provider);
            println!("=== LeLLM Agent — 天气查询链（真实 wttr.in API）===\n");
            let question = format!("帮我查一下{}的天气", address);
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block(question.clone()),
            }]);
            observe_react_loop(stream, &question).await
        }
        _ => {
            let agent = create_weather_agent(provider);
            println!("=== LeLLM Agent — 天气查询链（真实 wttr.in API）===\n");
            let question = "帮我查一下浦东新区的天气";
            let stream = agent.execute_stream(vec![lellm_core::Message::User {
                content: lellm_core::text_block(question.to_string()),
            }]);
            observe_react_loop(stream, question).await
        }
    }
}
