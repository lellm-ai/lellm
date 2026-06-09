//! 工具调用 — 使用真实 Provider 的 ReAct 循环（纯 http_get 版本）
//!
//! 天气查询链：LLM 推理城市名 → `http_get` wttr.in → 解析为 JSON
//! 核心设计：工具层不硬编码业务 API，仅提供通用 `http_get`。
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example tool_use_react [地址]
//! ```

#[path = "_shared/shared.rs"]
mod shared;

use lellm_agent::{AgentBuilder, ToolArgs, ToolRegistration, ToolUseLoop, schemars::JsonSchema};
use lellm_core::{Message, ToolError, ToolErrorKind, text_block};
use lellm_macros::ToolDefinition;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::GenericProvider;
use lellm_provider::providers::openai_compat::OpenAICompatAdapter;
use std::sync::Arc;

// ─── 通用 HTTP GET 工具 ─────────────────────────────────────────

#[derive(JsonSchema, ToolDefinition)]
#[tool(
    name = "http_get",
    description = "发送 HTTP GET 请求并返回响应文本。URL 由你根据 API 文档构造。"
)]
#[allow(dead_code)]
struct HttpGetArgs {
    /// 完整的请求 URL
    url: String,
}

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

fn register_http_tools() -> Vec<ToolRegistration> {
    vec![ToolRegistration::safe(
        HttpGetArgs::tool_definition(),
        |args| {
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
                        message: format!("任务失败: {e}"),
                    })?
            }
        },
    )]
}

// ─── Agent 工厂 ─────────────────────────────────────────────────

fn create_agent(provider: GenericProvider<OpenAICompatAdapter>) -> ToolUseLoop {
    let prompt = r#"你是天气查询助手。

任务：
1. 将地址映射为 wttr.in 城市名（自己推理）
2. 调用 http_get(https://wttr.in/{city}?format=%c+%t+%h+%w)
3. 输出 JSON

地址示例：浦东→shanghai, 新宿→tokyo, 曼哈顿→new-york, 未知→unknown

输出格式（纯 JSON）：
单地址: {"city":"tokyo","city_source":"新宿","condition":"小雨","temperature":"17°C","humidity":"94%","wind":"7km/h"}
多地址: [{"city":"tokyo","city_source":"新宿",...},{"city":"new-york","city_source":"曼哈顿",...}]

规则：
- 无法确定返回 unknown
- 最终回答必须为纯 JSON，禁止解释"#;

    AgentBuilder::new(ResolvedModel {
        provider: Arc::new(provider),
        model: "Qwen3.6".to_string(),
        context_window: None,
    })
    .system_prompt(prompt.to_string())
    .tools(register_http_tools())
    .max_iterations(5)
    .max_output_tokens(2000)
    .max_reasoning_tokens(8000)
    .build()
}

// ─── 主函数 ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lellm_agent=debug,lellm_provider=debug,info".into()),
        )
        .try_init();

    let provider = GenericProvider::from_env(OpenAICompatAdapter::llama())
        .expect("OpenAI provider env error");
    let agent = create_agent(provider);

    println!("=== LeLLM Agent — 天气查询链（纯 http_get）===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下陆家嘴/新宿/阿尔卡吉/奇台的天气".to_string(),
    };

    let stream = agent.execute_stream(vec![Message::User {
        content: text_block(question.clone()),
    }]);
    shared::observe_react_loop(stream, &question).await
}
