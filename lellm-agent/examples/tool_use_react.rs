//! 工具调用 — 使用真实 Provider 的 ReAct 循环
//!
//! 天气查询链：LLM 推理城市 → 构造 wttr.in URL → `http_get` → 失败重试
//!
//! 核心设计：**工具层不硬编码任何业务 API**，仅提供通用 `http_get`。
//! LLM 根据 system_prompt 中的 API 知识，自行推理请求 URL。
//!
//! ReAct 流程：
//! ```text
//! 用户: "帮我查一下浦东新区的天气"
//!   → LLM 推理: 浦东新区 → 上海 → shanghai
//!   → LLM 构造: https://wttr.in/shanghai?format=j1
//!   → 调用: http_get(url)
//!   → ✅ 成功 → 解析 JSON → 自然语言总结
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
        "你是一个天气查询助手，拥有以下 API 知识：\n\
         \n\
         【wttr.in 天气 API】\n\
         - 简洁格式：https://wttr.in/{城市}?format=%c+%t+%h+%w\n\
         - 返回：天气状况 温度 湿度 风速（空格分隔），如 '小雨 17°C 94% 7km/h'\n\
         - 城市未找到时返回空响应\n\
         \n\
         工作流程：\n\
         1. 用户给出地址 → 推理对应城市英文名（小写，多词用连字符，如 new-york）\n\
         2. 构造 wttr.in URL，调用 http_get(url)\n\
         3. 若返回空或错误 → 重新推理城市名 → 再次调用 http_get\n\
         4. 获取天气后，用自然语言总结\n\
         \n\
         注意：直接调用 http_get 工具，不要尝试其他方式获取天气。"
            .to_string(),
    )
    .tools(register_http_tools())
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

    println!("=== LeLLM Agent — 天气查询链（LLM 推理 + 通用 http_get）===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下金桥的天气".to_string(),
    };

    let stream = agent.execute_stream(vec![Message::User {
        content: text_block(question.clone()),
    }]);
    observe_react_loop(stream, &question).await
}
