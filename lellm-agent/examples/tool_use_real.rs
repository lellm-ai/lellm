//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 对应 LangChain 用法：
//! ```python
//! from langchain.tools import tool
//! from langchain.agents import create_agent
//!
//! @tool
//! def search_products(query: str) -> str:
//!     """搜索产品目录，返回匹配的产品列表。"""
//!     return f"找到产品: {query}"
//!
//! @tool
//! def check_inventory(product_id: str) -> str:
//!     """检查指定产品的库存数量。"""
//!     return f"{product_id}: 库存 10 件"
//!
//! agent = create_agent(model, tools=[search_products, check_inventory])
//! result = agent.invoke("找出当前最受欢迎的无线耳机并检查其库存")
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

// ─── 定义工具 ───────────────────────────────────────────────────

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

#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "check_inventory", description = "检查指定产品的库存数量")]
struct CheckInventoryArgs {
    /// 产品 ID
    product_id: String,
}

#[allow(dead_code)]
#[derive(JsonSchema, ToolDefinitionDerive)]
#[tool(name = "get_weather", description = "获取指定位置的天气信息")]
struct GetWeatherArgs {
    /// 城市或地点名称
    location: String,
}

// ─── 工具实现 ───────────────────────────────────────────────────

/// 注册所有工具
fn register_tools() -> Vec<ToolRegistration> {
    vec![
        // 产品搜索工具
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
                    q if q.contains("rust") => {
                        format!(
                            "搜索结果：{}\nRust 是一门系统编程语言，专注于内存安全和并发性能。",
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
        // 库存检查工具
        ToolRegistration::safe(CheckInventoryArgs::tool_definition(), |args| {
            let product_id = args
                .get("product_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                let inventory = match product_id.to_lowercase().as_str() {
                    pid if pid.contains("wh-1000xm5") => Ok("产品 WH-1000XM5：库存 10 件，预计明日发货".to_string()),
                    pid if pid.contains("airpods") => Ok("产品 AirPods Pro：库存 25 件，预计今日发货".to_string()),
                    pid if pid.contains("qc45") => Ok("产品 QC45：库存 8 件，预计两日内发货".to_string()),
                    _ => Err(ToolError {
                        kind: ToolErrorKind::NotFound,
                        message: format!("产品 {} 未找到，请确认产品 ID 是否正确。", product_id),
                    }),
                };
                inventory
            }
        }),
        // 天气工具
        ToolRegistration::safe(GetWeatherArgs::tool_definition(), |args| {
            let location = args
                .get("location")
                .and_then(|v| v.as_str())
                .unwrap_or("")
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
                Ok(format!(
                    "{} 的天气：{}，温度 {}°C，湿度 {}%",
                    location, weather.1, weather.0, weather.2
                ))
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

/// 创建带工具的 Agent
fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let model = ResolvedModel {
        provider: Arc::new(provider),
        model: "gpt-4o".to_string(),
    };

    AgentBuilder::new(model)
        .system_prompt(
            "你是一个有帮助的助手。你可以使用搜索产品、检查库存和天气查询工具来帮助用户。\
             在回答前，请先使用工具获取所需信息，再给出最终答案。"
                .to_string(),
        )
        .tools(register_tools())
        .max_iterations(10)
        .build()
}

// ─── ReAct 循环观测器 ───────────────────────────────────────────

/// 当前 ReAct 轮次的中间状态。
#[derive(Debug, Default)]
struct RoundState {
    /// 本轮 LLM 输出的推理文本（Token 累积）
    reasoning: String,
    /// ResponseComplete 携带的工具调用
    pending_tool_calls: Vec<lellm_core::ToolCall>,
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
///
/// 输出格式：
/// ```text
/// ================================== 人类消息 ==================================
/// 用户问题
///
/// ================================== AI 消息 ==================================
/// [推理文本]
/// 工具调用：
///   search_products (call_xxx)
///   参数: {"query": "..."}
///
/// ================================ 工具观察 ================================
/// 找到 5 个匹配...
///
/// ...（循环）...
///
/// ================================== AI 消息 ==================================
/// [最终答案]
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
                round.pending_tool_calls = tool_calls;

                if round.pending_tool_calls.is_empty() {
                    // 无工具调用 — AI 给出了最终答案
                    // （推理文本已通过 Token 流实时输出）
                    eprintln!("\n[DEBUG] >>> 第 {} 轮 — 最终回答", iteration);
                    let _ = usage; // 最终用量在 LoopEnd 中获取
                } else {
                    // 有工具调用 — 打印工具调用详情
                    println!();
                    println!(
                        "================================== AI 消息 =================================="
                    );
                    if !round.reasoning.is_empty() {
                        println!("推理: {}", round.reasoning);
                    }
                    println!("工具调用：");
                    for tc in &round.pending_tool_calls {
                        println!("  {} ({})", tc.name, tc.id);
                        println!("  参数: {}", tc.arguments);
                    }
                    println!();
                    eprintln!(
                        "[DEBUG] >>> 第 {} 轮 — {} 个工具调用",
                        iteration,
                        round.pending_tool_calls.len()
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

                let observation = match &result {
                    Ok(output) => {
                        let output = output.clone();
                        (tool_name, Ok(output))
                    }
                    Err(err) => {
                        let err = err.clone();
                        (tool_name, Err(err))
                    }
                };
                round.tool_observations.push(observation);

                // 实时打印工具观察结果
                println!(
                    "=============================== 工具观察 ================================"
                );
                match &round.tool_observations.last().unwrap().1 {
                    Ok(output) => {
                        println!("{}", output);
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

    // Stream 意外结束（未收到 LoopEnd 或 LoopError）
    eprintln!("[WARN] Stream 意外结束，未收到终止事件");
    Ok(())
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
        .unwrap_or_else(|| "找出当前最受欢迎的无线耳机并检查其库存".to_string());

    // 使用流式执行，实时观测 ReAct 循环
    let stream = agent.execute_stream(vec![lellm_core::Message::User {
        content: lellm_core::text_block(question.clone()),
    }]);

    observe_react_loop(stream, &question).await
}
