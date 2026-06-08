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
use lellm_provider::providers::base::GenericProvider;
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
    GenericProvider::from_env(OpenAICompatAdapter::openai()).expect("OpenAI provider env error")
}

/// 其实应该拆成 tool resolve_city(address)
/*
    LLM负责：

输入地址
↓
调用 resolve_city
↓
调用 wttr
↓
返回 JSON

这样：

Token 降低 90%+
不会出现 "阿尔卡吉→Alcatraz" 这种幻觉
不会浪费 reasoning budget
Agent Loop 更稳定

     */
fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let prompt = r#"你是天气查询助手。
任务分两步：

步骤1：地址归一化

将用户输入地址映射为 wttr.in 可识别城市。

规则：

- 仅允许输出一个城市
- 不允许多个候选
- 不允许猜测
- 不允许解释
- 不允许分析过程
- 无法确定时返回 unknown

示例：

宁海 -> ningbo
浦东 -> shanghai
新宿 -> tokyo
未知地点 -> unknown

步骤2：天气查询

仅对非 unknown 城市调用 http_get：

https://wttr.in/{city}?format=%c+%t+%h+%w

失败处理：

- 最多允许一个备用城市
- 仅重试一次
- 再失败返回 unknown

最终输出：

单地址：

{
  "city":"tokyo",
  "city_source":"新宿",
  "condition":"小雨",
  "temperature":"17°C",
  "humidity":"94%",
  "wind":"7km/h"
}

多地址：

[
  {...},
  {...}
]

最终回答必须为 JSON。
禁止输出解释、分析、思考过程。
地址推理属于简单映射任务。

禁止进行地理分析。
禁止进行多轮推理。
禁止生成 reasoning。
Think less.
Use direct mapping only."#;

    AgentBuilder::new(ResolvedModel {
        provider: Arc::new(provider),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system_prompt(prompt.to_string())
    .tools(register_http_tools())
    .max_iterations(10)
    .max_output_tokens(2000)
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
            }

            AgentEvent::Provider(ProviderEvent::Token { token }) => {
                round.reasoning.push_str(&token);
                print!("{token}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }

            AgentEvent::Provider(ProviderEvent::ThinkingDelta { thinking, .. }) => {
                round.reasoning.push_str(&thinking);
            }

            AgentEvent::Provider(ProviderEvent::ResponseComplete { tool_calls, usage }) => {
                let elapsed = round
                    .step_start
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                step_times.push((iteration, elapsed));

                if tool_calls.is_empty() {
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
