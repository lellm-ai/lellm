//! weather_agent_prompt — 天气查询链（纯 http_get 版本）
//!
//! 天气查询链：LLM 推理城市名 → `http_get` wttr.in → 解析为 JSON
//! 核心设计：工具层不硬编码业务 API，仅提供通用 `http_get`。
//!
//! ```text
//! OPENAI_API_KEY=sk-xxx cargo run --example weather_agent_prompt [地址]
//! ```

#[path = "_shared/shared.rs"]
mod shared;

use lellm_agent::AgentBuilder;
use lellm_core::{Message, Prompt, ToolError, ToolErrorKind, ToolResult, text_block};
use lellm_derive::tool;
use lellm_provider::ResolvedModel;
use lellm_provider::providers::base::CodecProvider;
use lellm_provider::providers::openai_compat::OpenAICompatCodec;
// ─── 通用 HTTP GET 工具 ─────────────────────────────────────────

/// 发送 HTTP GET 请求并返回响应文本。URL 由你根据 API 文档构造。
#[tool(
    name = "http_get",
    description = "发送 HTTP GET 请求并返回响应文本。URL 由你根据 API 文档构造。"
)]
async fn http_get(url: String) -> ToolResult {
    let body = tokio::task::spawn_blocking(move || {
        reqwest::blocking::get(&url)
            .map_err(|e| ToolError {
                kind: ToolErrorKind::Network,
                message: format!("请求失败: {e}"),
            })?
            .text()
            .map_err(|e| ToolError {
                kind: ToolErrorKind::Internal,
                message: format!("读取响应失败: {e}"),
            })
    })
    .await
    .map_err(|e| ToolError {
        kind: ToolErrorKind::Internal,
        message: format!("任务失败: {e}"),
    })??;
    Ok(serde_json::json!(body))
}

// ─── 分层 System Prompt — 最大化前缀缓存 ────────────────────────

/// 构建分层 Prompt，全部 cached — 用户查询通过 user message 传递。
///
/// 缓存策略：
/// - L1 核心身份：永不变化
/// - L2 任务步骤：极少变化
/// - L3 归一化规则：极少变化
/// - L4 城市示例：极少变化
/// - L5 天气查询步骤：极少变化
/// - L6 输出格式：极少变化（最后一个 cached layer → 获得 cache_control 断点）
///
/// 用户查询作为 user message 传递，不混入 system prompt。
/// 这样 system prompt 可以 100% 被前缀缓存。
fn build_system_prompt() -> Prompt {
    Prompt::new()
        // L1 — 核心身份
        .stable("你是天气查询助手。")
        // L2 — 任务步骤
        .stable(
            "任务分两步：

步骤1：地址归一化

将用户输入地址映射为 wttr.in 可识别城市。",
        )
        // L3 — 归一化规则
        .stable(
            "规则：

- 仅允许输出一个城市
- 不允许多个候选
- 不允许猜测
- 不允许解释
- 不允许分析过程
- 无法确定时返回 unknown",
        )
        // L4 — 城市示例
        .stable(
            "示例：

宁海 -> ningbo
浦东 -> shanghai
新宿 -> tokyo
未知地点 -> unknown",
        )
        // L5 — 天气查询步骤
        .stable(
            "步骤2：天气查询

仅对非 unknown 城市调用 http_get：

https://wttr.in/{city}?format=%c+%t+%h+%w

失败处理：

- 最多允许一个备用城市
- 仅重试一次
- 再失败返回 unknown",
        )
        // L6 — 输出格式 + 约束规则（最后一个 stable → 获得断点 ✓）
        .stable(
            "最终输出：

单地址：

{
  \"city\":\"tokyo\",
  \"address\":\"新宿\",
  \"condition\":\"小雨\",
  \"temperature\":\"17°C\",
  \"humidity\":\"94%\",
  \"wind\":\"7km/h\"
}

多地址：

[{...},{...}]

最终回答必须为紧凑JSON。
禁止输出解释、分析、思考过程。
地址推理属于简单映射任务。
禁止进行地理分析。
禁止进行多轮推理。
禁止生成 reasoning。",
        )
}

// ─── Agent 工厂 ─────────────────────────────────────────────────

fn create_agent(provider: CodecProvider<OpenAICompatCodec>) -> lellm_agent::ToolUseLoop {
    AgentBuilder::new(ResolvedModel::new(provider, "Qwen3.6"))
        .system(build_system_prompt())
        .tool(http_get_tool())
        // ToolCachePolicy::Auto（默认）— 工具定义自动获得 cache_control 断点
        .max_iterations(5)
        .max_output_tokens(8000)
        .reasoning_budget(8000)
        //.reasoning(lellm_core::ReasoningConfig::Disabled)
        .compile()
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

    let provider =
        CodecProvider::load(OpenAICompatCodec::llama()).expect("OpenAI provider env error");

    println!("=== LeLLM Agent — 天气查询链（纯 http_get）===\n");

    let question = match std::env::args().nth(1) {
        Some(addr) => format!("帮我查一下{addr}的天气"),
        None => "帮我查一下陆家嘴/新宿/阿尔卡吉/奇台的天气".to_string(),
    };

    // System prompt 全部缓存（无动态层），查询通过 user message 传递
    let agent = create_agent(provider);

    let messages = vec![Message::user(text_block(question.clone()))];
    let stream = agent.invoke_stream(messages);
    shared::observe_react_loop(stream, &question).await
}
