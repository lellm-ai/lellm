//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 天气查询链：LLM 推理城市 → `http_get` wttr.in 文本 → 解析为 JSON 输出
//!
//! 核心设计：**工具层不硬编码任何业务 API**，仅提供通用 `http_get`。
//! LLM 根据 system_prompt 中的 API 知识，自行构造 URL，解析轻量文本响应，
//! 并以 JSON 格式输出最终结果。
//!
//! 单地址流程：
//! ```text
//! 用户: "帮我查一下浦东新区的天气"
//!   → LLM 推理: 浦东新区 → 上海 → shanghai
//!   → LLM 构造: https://wttr.in/shanghai?format=%c+%t+%h+%w
//!   → 调用: http_get(url) → 返回 '小雨 17°C 94% 7km/h'
//!   → LLM 解析 → {"city":"shanghai","city_source":"浦东新区","condition":"小雨",...}
//! ```
//!
//! 多地址流程（并行工具调用）：
//! ```text
//! 用户: "查一下东京和纽约的天气"
//!   → LLM 推理: 东京→tokyo, 纽约→new-york
//!   → 并行调用: http_get(tokyo) + http_get(new-york)
//!   → LLM 汇总 → [{"city":"tokyo","city_source":"东京",...},{"city":"new-york",...}]
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

// ─── 通用 HTTP GET 工具 ─────────────────────────────────────────

/// 通用 HTTP GET 请求参数
#[derive(JsonSchema, ToolDefinition)]
#[tool(
    name = "http_get",
    description = "发送 HTTP GET 请求并返回响应文本。\
                   用于调用外部 API 获取数据。URL 必须由你根据 API 文档构造。"
)]
#[allow(dead_code)]
struct HttpGetArgs {
    /// 完整的请求 URL（包含协议、域名、路径、查询参数）
    url: String,
}

/// 通用 HTTP GET — 纯传输层，不知业务语义
fn http_get(url: &str) -> Result<String, ToolError> {
    reqwest::blocking::get(url)
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Network,
            message: format!("请求失败: {e}"),
        })?
        .text()
        .map_err(|e| ToolError {
            kind: ToolErrorKind::Internal,
            message: format!("读取响应失败: {e}"),
        })
}

/// 注册通用 HTTP 工具
fn register_http_tools() -> Vec<ToolRegistration> {
    vec![ToolRegistration::safe(
        HttpGetArgs::tool_definition(),
        |args: &serde_json::Value| {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            async move {
                tokio::task::spawn_blocking(move || http_get(&url))
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
            &std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            std::env::var("OPENAI_API_KEY").expect("请设置 OPENAI_API_KEY"),
        )
        .expect("Invalid base URL")
        .with_connect_timeout(std::time::Duration::from_secs(10))
        .with_timeout(std::time::Duration::from_secs(
            std::env::var("OPENAI_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
        ))
        .with_idle_timeout(std::time::Duration::from_secs(30)),
    )
}

fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    AgentBuilder::new(ResolvedModel {
        provider: Arc::new(provider),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system_prompt(
        "天气查询助手。使用 wttr.in 获取天气：\n\
         \n\
         URL 模板：https://wttr.in/{city}?format=%c+%t+%h+%w\n\
         城市名：英文小写，多词用连字符（new-york, san-francisco）\n\
         **重要**：地址请推理到地级市（如 宁海→宁波→ningbo，浦东→上海→shanghai）\n\
         返回：空格分隔的文本，如 '小雨 17°C 94% 7km/h'\n\
         \n\
         单地址：推理城市→调用 http_get→解析→输出 JSON\n\
         多地址：推理所有城市→并行调用多个 http_get→汇总为 JSON 数组\n\
         \n\
         JSON 格式（每个对象）：\n\
         {{\"city\":\"tokyo\",\"city_source\":\"新宿\",\"condition\":\"小雨\",\"temperature\":\"17°C\",\"humidity\":\"94%\",\"wind\":\"7km/h\"}}\n\
         city_source 为用户原始输入地址。多地址时输出 JSON 数组。\n\
         \n\
         若 API 报错或返回空→重新推理城市名→重试。\n\
         最终回答必须是 JSON，不要其他文字。"
            .to_string(),
    )
    .tools(register_http_tools())
    .max_iterations(10)
    .max_output_tokens(4000)
    .build()
}

// ─── ReAct 循环观测器 ───────────────────────────────────────────

/// 当前 ReAct 轮次的中间状态
#[derive(Debug, Default)]
struct RoundState {
    reasoning: String,
    current_tool: Option<String>,
    step_start: Option<std::time::Instant>,
}

async fn observe_react_loop(
    mut stream: AgentStream,
    question: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let total_start = std::time::Instant::now();

    println!("================================ 人类消息 =================================");
    println!("{question}\n");

    let mut iteration: usize = 0;
    let mut round = RoundState::default();
    let mut step_times: Vec<(usize, f64)> = Vec::new();

    while let Some(event) = stream.recv().await {
        match event {
            // ─── Provider 事件 ───────────────────────────────────────
            AgentEvent::Provider(ProviderEvent::Start { model }) => {
                iteration += 1;
                round = RoundState {
                    reasoning: String::new(),
                    current_tool: None,
                    step_start: Some(std::time::Instant::now()),
                };
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
                let elapsed = round
                    .step_start
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                step_times.push((iteration, elapsed));

                if tool_calls.is_empty() {
                    eprintln!(
                        "\n[DEBUG] >>> 第 {iteration} 轮 — 最终回答 ({:.2}s)",
                        elapsed
                    );
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
                        "[DEBUG] >>> 第 {iteration} 轮 — {} 个工具调用 ({:.2}s)",
                        tool_calls.len(),
                        elapsed
                    );
                }
            }

            // ─── 工具事件 ────────────────────────────────────────────
            AgentEvent::ToolStart { name, .. } => {
                round.current_tool = Some(name);
            }

            AgentEvent::ToolEnd { result, .. } => {
                let tool_elapsed = round
                    .step_start
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
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
                eprintln!("[DEBUG] >>> 工具执行耗时 {:.2}s", tool_elapsed);
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

            AgentEvent::ContextCompacted {
                before_tokens,
                after_tokens,
                removed_messages,
            } => {
                println!("============================ 上下文压缩 ==============================");
                println!(
                    "📦 上下文压缩: {before_tokens} → {after_tokens} tokens (移除 {removed_messages} 条消息)"
                );
                eprintln!(
                    "[DEBUG] >>> 上下文压缩: {before_tokens} → {after_tokens} tokens, 移除 {removed_messages} 条消息"
                );
                println!();
            }

            // ─── 终态事件 ────────────────────────────────────────────
            AgentEvent::LoopEnd { result } => {
                let total = total_start.elapsed();
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
                println!();
                println!("--- 耗时明细 ---");
                for (i, t) in &step_times {
                    println!("  第 {} 轮: {:.2}s", i, t);
                }
                println!("总耗时: {:.2}s", total.as_secs_f64());
                return Ok(());
            }

            AgentEvent::LoopError { error, iterations } => {
                let total = total_start.elapsed();
                println!();
                println!("================================ 错误 =================================");
                println!("❌ Agent 执行失败（第 {iterations} 轮）: {error}");
                println!("总耗时: {:.2}s", total.as_secs_f64());
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
    yunli::setup_logger_debug().unwrap();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=debug,lellm_provider=debug,info".into()),
        )
        .try_init();

    if std::env::var("OPENAI_API_KEY").is_err() {
        eprintln!("错误：请设置 OPENAI_API_KEY 环境变量");
        eprintln!("用法：OPENAI_API_KEY=sk-xxx cargo run --example tool_use_react [地址]");
        std::process::exit(1);
    }

    let provider = create_provider();
    let agent = create_agent(provider);

    println!("=== LeLLM Agent — 天气查询链（http_get + JSON 输出）===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => {
            "帮我查一下陆家嘴潍坊街道/新宿/阿尔卡吉/奇台/龙爱路云视路/云锦东方的天气".to_string()
        }
    };

    let stream = agent.execute_stream(vec![Message::User {
        content: text_block(question.clone()),
    }]);
    observe_react_loop(stream, &question).await
}
